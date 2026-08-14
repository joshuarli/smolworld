use crate::model::{gateway_mac, WorldAllocationState, WorldConfig};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

const DEFAULT_UPSTREAM_DNS: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

pub(crate) struct Gateway {
    pub(crate) ip: Ipv4Addr,
    pub(crate) mac: [u8; 6],
    records: HashMap<String, Ipv4Addr>,
    upstream_dns: Option<Ipv4Addr>,
}

impl Gateway {
    pub(crate) fn new(config: &WorldConfig, state: &WorldAllocationState) -> Self {
        let mut records = HashMap::new();
        for name in config.machines.keys() {
            let ip = state.assignments.get(name).expect("allocated machine").ip;
            records.insert(name.clone(), ip);
            records.insert(format!("{name}.{}", config.network.domain), ip);
        }
        Self {
            ip: config.network.gateway,
            mac: gateway_mac(),
            records,
            upstream_dns: config.network.egress.then_some(DEFAULT_UPSTREAM_DNS),
        }
    }

    pub(crate) fn handle(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 14 {
            return None;
        }
        match u16::from_be_bytes([frame[12], frame[13]]) {
            0x0806 => self.arp_reply(frame),
            0x0800 => self.dns_reply(frame),
            _ => None,
        }
    }

    pub(crate) fn arp_reply(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 42
            || frame[14..16] != [0, 1]
            || frame[16..18] != [0x08, 0]
            || frame[18] != 6
            || frame[19] != 4
            || frame[20..22] != [0, 1]
            || frame[38..42] != self.ip.octets()
        {
            return None;
        }
        let mut reply = vec![0; 42];
        reply[..6].copy_from_slice(&frame[6..12]);
        reply[6..12].copy_from_slice(&self.mac);
        reply[12..14].copy_from_slice(&[0x08, 0x06]);
        reply[14..22].copy_from_slice(&[0, 1, 0x08, 0, 6, 4, 0, 2]);
        reply[22..28].copy_from_slice(&self.mac);
        reply[28..32].copy_from_slice(&self.ip.octets());
        reply[32..38].copy_from_slice(&frame[22..28]);
        reply[38..42].copy_from_slice(&frame[28..32]);
        Some(reply)
    }

    pub(crate) fn dns_reply(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 14 + 20 + 8 {
            return None;
        }
        let ip_start = 14;
        let version_ihl = frame[ip_start];
        if version_ihl >> 4 != 4 {
            return None;
        }
        let ip_len = usize::from(version_ihl & 0x0f) * 4;
        if ip_len < 20 || frame.len() < ip_start + ip_len + 8 || frame[ip_start + 9] != 17 {
            return None;
        }
        let total_len = usize::from(u16::from_be_bytes([
            frame[ip_start + 2],
            frame[ip_start + 3],
        ]));
        if total_len < ip_len + 8 || frame.len() < ip_start + total_len {
            return None;
        }
        let destination = Ipv4Addr::new(
            frame[ip_start + 16],
            frame[ip_start + 17],
            frame[ip_start + 18],
            frame[ip_start + 19],
        );
        if destination != self.ip {
            return None;
        }
        let udp_start = ip_start + ip_len;
        if u16::from_be_bytes([frame[udp_start + 2], frame[udp_start + 3]]) != 53 {
            return None;
        }
        let udp_len = usize::from(u16::from_be_bytes([
            frame[udp_start + 4],
            frame[udp_start + 5],
        ]));
        if udp_len < 8 || udp_start + udp_len > ip_start + total_len {
            return None;
        }
        let dns = &frame[udp_start + 8..udp_start + udp_len];
        let (question_end, name, query_type, query_class) = parse_dns_question(dns)?;
        let answer_ip = if query_type == 1 && query_class == 1 {
            self.records.get(&name).copied()
        } else {
            None
        };
        let known_name = self.records.contains_key(&name);
        let response_dns = if !known_name {
            match self.upstream_dns {
                Some(upstream) => forward_dns_query(dns, upstream)
                    .unwrap_or_else(|| synthetic_dns_error(dns, question_end, 2)),
                None => synthetic_dns_error(dns, question_end, 3),
            }
        } else {
            let mut response_dns = Vec::with_capacity(question_end + 16);
            response_dns.extend_from_slice(&dns[..2]);
            response_dns.extend_from_slice(&0x8180_u16.to_be_bytes());
            response_dns.extend_from_slice(&1_u16.to_be_bytes());
            response_dns.extend_from_slice(&(u16::from(answer_ip.is_some())).to_be_bytes());
            response_dns.extend_from_slice(&0_u16.to_be_bytes());
            response_dns.extend_from_slice(&0_u16.to_be_bytes());
            response_dns.extend_from_slice(&dns[12..question_end]);
            if let Some(ip) = answer_ip {
                response_dns.extend_from_slice(&[0xc0, 0x0c]);
                response_dns.extend_from_slice(&1_u16.to_be_bytes());
                response_dns.extend_from_slice(&1_u16.to_be_bytes());
                response_dns.extend_from_slice(&60_u32.to_be_bytes());
                response_dns.extend_from_slice(&4_u16.to_be_bytes());
                response_dns.extend_from_slice(&ip.octets());
            }
            response_dns
        };

        let source_ip = [
            frame[ip_start + 12],
            frame[ip_start + 13],
            frame[ip_start + 14],
            frame[ip_start + 15],
        ];
        let source_port = [frame[udp_start], frame[udp_start + 1]];
        build_udp_ipv4_ethernet_reply(
            frame,
            self.mac,
            self.ip,
            source_ip,
            source_port,
            &response_dns,
        )
    }
}

fn forward_dns_query(query: &[u8], upstream: Ipv4Addr) -> Option<Vec<u8>> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    socket
        .send_to(query, SocketAddr::from((upstream, 53)))
        .ok()?;
    let mut response = [0_u8; 4096];
    let (size, _) = socket.recv_from(&mut response).ok()?;
    let response = &response[..size];
    if response.len() < 12 || response[..2] != query[..2] || response[2] & 0x80 == 0 {
        return None;
    }
    Some(response.to_vec())
}

fn synthetic_dns_error(query: &[u8], question_end: usize, rcode: u16) -> Vec<u8> {
    let mut response = Vec::with_capacity(question_end + 12);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&(0x8180_u16 | rcode).to_be_bytes());
    response.extend_from_slice(&1_u16.to_be_bytes());
    response.extend_from_slice(&0_u16.to_be_bytes());
    response.extend_from_slice(&0_u16.to_be_bytes());
    response.extend_from_slice(&0_u16.to_be_bytes());
    response.extend_from_slice(&query[12..question_end]);
    response
}

pub(crate) fn parse_dns_question(dns: &[u8]) -> Option<(usize, String, u16, u16)> {
    if dns.len() < 17 || u16::from_be_bytes([dns[4], dns[5]]) != 1 {
        return None;
    }
    let mut index = 12;
    let mut labels = Vec::new();
    loop {
        let length = usize::from(*dns.get(index)?);
        index += 1;
        if length == 0 {
            break;
        }
        if length > 63 || index + length > dns.len() {
            return None;
        }
        let label = std::str::from_utf8(&dns[index..index + length])
            .ok()?
            .to_ascii_lowercase();
        labels.push(label);
        index += length;
    }
    if index + 4 > dns.len() {
        return None;
    }
    let query_type = u16::from_be_bytes([dns[index], dns[index + 1]]);
    let query_class = u16::from_be_bytes([dns[index + 2], dns[index + 3]]);
    Some((index + 4, labels.join("."), query_type, query_class))
}

pub(crate) fn build_udp_ipv4_ethernet_reply(
    request: &[u8],
    source_mac: [u8; 6],
    source_ip: Ipv4Addr,
    destination_ip: [u8; 4],
    destination_port: [u8; 2],
    payload: &[u8],
) -> Option<Vec<u8>> {
    let udp_len = 8 + payload.len();
    let total_len = 20 + udp_len;
    if total_len > u16::MAX as usize {
        return None;
    }
    let mut reply = vec![0; 14 + total_len];
    reply[..6].copy_from_slice(&request[6..12]);
    reply[6..12].copy_from_slice(&source_mac);
    reply[12..14].copy_from_slice(&[0x08, 0]);
    let ip_start = 14;
    reply[ip_start] = 0x45;
    reply[ip_start + 2..ip_start + 4].copy_from_slice(&(total_len as u16).to_be_bytes());
    reply[ip_start + 6..ip_start + 8].copy_from_slice(&0x4000_u16.to_be_bytes());
    reply[ip_start + 8] = 64;
    reply[ip_start + 9] = 17;
    reply[ip_start + 12..ip_start + 16].copy_from_slice(&source_ip.octets());
    reply[ip_start + 16..ip_start + 20].copy_from_slice(&destination_ip);
    let checksum = ipv4_checksum(&reply[ip_start..ip_start + 20]);
    reply[ip_start + 10..ip_start + 12].copy_from_slice(&checksum.to_be_bytes());
    let udp_start = ip_start + 20;
    reply[udp_start..udp_start + 2].copy_from_slice(&53_u16.to_be_bytes());
    reply[udp_start + 2..udp_start + 4].copy_from_slice(&destination_port);
    reply[udp_start + 4..udp_start + 6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    reply[udp_start + 8..].copy_from_slice(payload);
    Some(reply)
}

pub(crate) fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum = 0_u32;
    for chunk in header.chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use crate::model::{Assignment, WorldAllocationState};
    use std::collections::BTreeMap;

    #[test]
    fn answers_arp_and_dns_a() {
        let config = parse_config(
            r#"
format: 2
world:
  name: demo
network:
  subnet: 10.89.0.0/24
machines:
  redis:
    smolfile: ./redis.Smolfile
  client:
    smolfile: ./client.Smolfile
"#,
        )
        .unwrap();
        let state = WorldAllocationState {
            seed: 7,
            assignments: BTreeMap::from([
                (
                    "redis".to_string(),
                    Assignment {
                        ip: "10.89.0.2".parse().unwrap(),
                        mac: [2, 0, 0, 0, 0, 2],
                        smolvm_name: "smw-v2-redis".to_string(),
                    },
                ),
                (
                    "client".to_string(),
                    Assignment {
                        ip: "10.89.0.3".parse().unwrap(),
                        mac: [2, 0, 0, 0, 0, 3],
                        smolvm_name: "smw-v2-client".to_string(),
                    },
                ),
            ]),
        };
        let gateway = Gateway::new(&config, &state);
        let client_mac = [2, 0, 0, 0, 0, 9];
        let client_ip = [10, 89, 0, 9];
        let mut arp = vec![0; 42];
        arp[..6].copy_from_slice(&[0xff; 6]);
        arp[6..12].copy_from_slice(&client_mac);
        arp[12..14].copy_from_slice(&[8, 6]);
        arp[14..22].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
        arp[22..28].copy_from_slice(&client_mac);
        arp[28..32].copy_from_slice(&client_ip);
        arp[38..42].copy_from_slice(&gateway.ip.octets());
        let reply = gateway.handle(&arp).unwrap();
        assert_eq!(&reply[20..22], &[0, 2]);
        assert_eq!(&reply[22..28], &gateway.mac);

        let request = dns_request(client_mac, client_ip, gateway.mac, gateway.ip, "redis");
        let reply = gateway.handle(&request).unwrap();
        assert_eq!(&reply[14 + 20 + 8 + 4..14 + 20 + 8 + 8], &[0, 1, 0, 1]);
        assert!(reply.ends_with(&state.assignments["redis"].ip.octets()));
    }

    #[test]
    fn ignores_truncated_and_malformed_ip_or_dns_frames() {
        let config = parse_config(
            r#"
format: 2
world:
  name: demo
network:
  subnet: 10.89.0.0/24
machines:
  redis:
    smolfile: ./redis.Smolfile
"#,
        )
        .unwrap();
        let state = WorldAllocationState {
            seed: 7,
            assignments: BTreeMap::from([(
                "redis".to_string(),
                Assignment {
                    ip: "10.89.0.2".parse().unwrap(),
                    mac: [2, 0, 0, 0, 0, 2],
                    smolvm_name: "smw-v2-redis".to_string(),
                },
            )]),
        };
        let gateway = Gateway::new(&config, &state);
        assert_eq!(gateway.handle(&[]), None);
        assert_eq!(gateway.handle(&[0; 41]), None);

        let request = dns_request(
            [2, 0, 0, 0, 0, 9],
            [10, 89, 0, 9],
            gateway.mac,
            gateway.ip,
            "redis",
        );
        assert_eq!(gateway.handle(&request[..request.len() - 1]), None);

        let mut invalid_protocol = request.clone();
        invalid_protocol[23] = 6;
        assert_eq!(gateway.handle(&invalid_protocol), None);

        let mut invalid_total_length = request;
        invalid_total_length[16..18].copy_from_slice(&u16::MAX.to_be_bytes());
        assert_eq!(gateway.handle(&invalid_total_length), None);
    }

    fn dns_request(
        client_mac: [u8; 6],
        client_ip: [u8; 4],
        gateway_mac: [u8; 6],
        gateway_ip: Ipv4Addr,
        name: &str,
    ) -> Vec<u8> {
        let mut dns = vec![0x12, 0x34, 0x01, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            dns.push(label.len() as u8);
            dns.extend_from_slice(label.as_bytes());
        }
        dns.extend_from_slice(&[0, 0, 1, 0, 1]);
        let mut request = vec![0; 14 + 20 + 8 + dns.len()];
        request[..6].copy_from_slice(&gateway_mac);
        request[6..12].copy_from_slice(&client_mac);
        request[12..14].copy_from_slice(&[8, 0]);
        request[14] = 0x45;
        request[16..18].copy_from_slice(&((20 + 8 + dns.len()) as u16).to_be_bytes());
        request[22] = 64;
        request[23] = 17;
        request[26..30].copy_from_slice(&client_ip);
        request[30..34].copy_from_slice(&gateway_ip.octets());
        request[34..36].copy_from_slice(&12345_u16.to_be_bytes());
        request[36..38].copy_from_slice(&53_u16.to_be_bytes());
        request[38..40].copy_from_slice(&((8 + dns.len()) as u16).to_be_bytes());
        request[42..].copy_from_slice(&dns);
        request
    }
}
