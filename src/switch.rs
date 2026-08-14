use crate::gateway::Gateway;
use crate::model::{format_mac, WorldConfig, WorldState};
use crate::state::fnv1a;
use crate::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

const FRAME_MAX: usize = 65_535;
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) enum SwitchEvent {
    Attached {
        port: String,
        connection: u64,
        writer: Arc<Mutex<UnixStream>>,
    },
    Frame {
        port: String,
        connection: u64,
        frame: Vec<u8>,
    },
    Detached {
        port: String,
        connection: u64,
    },
    Shutdown,
}

/// Derive one deterministic listener socket inside an already-authorized
/// runtime directory. The caller owns versioning of that directory; the
/// switch deliberately has no knowledge of allocation-state formats.
pub(crate) fn port_socket_path(runtime_dir: &Path, machine: &str) -> PathBuf {
    runtime_dir.join(format!("p-{:012x}.sock", fnv1a(machine.as_bytes())))
}

pub(crate) fn spawn_port_acceptor(
    port: String,
    listener: UnixListener,
    switch_tx: mpsc::Sender<SwitchEvent>,
    attached_tx: mpsc::Sender<String>,
    shutdown: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("smolworld-port-{port}"))
        .spawn(move || {
            let mut next_connection = 0_u64;
            while !shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // The listener is nonblocking so shutdown remains
                        // responsive. Accepted Unix streams inherit that mode
                        // on macOS; frame reads must instead block until a
                        // complete header/payload arrives, otherwise an idle
                        // but healthy NIC is mistaken for a disconnected one.
                        if let Err(error) = prepare_port_stream(&stream) {
                            eprintln!("smolworld: configure port {port}: {error}");
                            return;
                        }
                        let writer = match stream.try_clone() {
                            Ok(writer) => Arc::new(Mutex::new(writer)),
                            Err(error) => {
                                eprintln!("smolworld: clone port {port}: {error}");
                                return;
                            }
                        };
                        next_connection = next_connection.wrapping_add(1);
                        // Zero is reserved as the impossible pre-attachment
                        // value, so a wrapping counter still names each live
                        // stream unambiguously in practice.
                        if next_connection == 0 {
                            next_connection = 1;
                        }
                        let connection = next_connection;
                        if switch_tx
                            .send(SwitchEvent::Attached {
                                port: port.clone(),
                                connection,
                                writer,
                            })
                            .is_err()
                        {
                            return;
                        }
                        let _ = attached_tx.send(port.clone());
                        let reader_port = port.clone();
                        let reader_tx = switch_tx.clone();
                        thread::spawn(move || {
                            read_port_frames(&reader_port, connection, stream, &reader_tx);
                            let _ = reader_tx.send(SwitchEvent::Detached {
                                port: reader_port,
                                connection,
                            });
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => {
                        eprintln!("smolworld: accept port {port}: {error}");
                        return;
                    }
                }
            }
        })
        .map_err(|error| format!("start listener thread: {error}"))
}

pub(crate) fn prepare_port_stream(stream: &UnixStream) -> io::Result<()> {
    stream.set_nonblocking(false)
}

pub(crate) fn read_port_frames(
    port: &str,
    connection: u64,
    mut stream: UnixStream,
    switch_tx: &mpsc::Sender<SwitchEvent>,
) {
    loop {
        let mut length = [0_u8; 4];
        if let Err(error) = stream.read_exact(&mut length) {
            if error.kind() != io::ErrorKind::UnexpectedEof {
                eprintln!("smolworld: read port {port}: {error}");
            }
            return;
        }
        let length = u32::from_be_bytes(length) as usize;
        if !(14..=FRAME_MAX).contains(&length) {
            eprintln!("smolworld: port {port} sent invalid Ethernet frame length {length}");
            return;
        }
        let mut frame = vec![0; length];
        if let Err(error) = stream.read_exact(&mut frame) {
            eprintln!("smolworld: read port {port} frame: {error}");
            return;
        }
        if switch_tx
            .send(SwitchEvent::Frame {
                port: port.to_string(),
                connection,
                frame,
            })
            .is_err()
        {
            return;
        }
    }
}

pub(crate) fn wait_for_attachments(
    receiver: &mpsc::Receiver<String>,
    config: &WorldConfig,
) -> Result<()> {
    let mut expected: HashSet<_> = config.machines.keys().cloned().collect();
    let deadline = std::time::Instant::now() + ATTACH_TIMEOUT;
    while !expected.is_empty() {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| {
                format!(
                    "timed out waiting for virtio-net attachment from {:?}",
                    expected
                )
            })?;
        let port = receiver.recv_timeout(remaining).map_err(|_| {
            format!(
                "timed out waiting for virtio-net attachment from {:?}",
                expected
            )
        })?;
        if expected.remove(&port) {
            eprintln!("smolworld: attached {port}");
        }
    }
    Ok(())
}

pub(crate) fn print_allocations(config: &WorldConfig, state: &WorldState) {
    println!("WORLD\t{}", config.name);
    println!("MACHINE\tIP\tMAC");
    for name in config.machines.keys() {
        let assignment = state.assignments.get(name).expect("allocated machine");
        println!("{name}\t{}\t{}", assignment.ip, format_mac(assignment.mac));
    }
}

pub(crate) fn run_switch(
    receiver: mpsc::Receiver<SwitchEvent>,
    gateway: Gateway,
    shutdown: Arc<AtomicBool>,
) {
    let mut ports: BTreeMap<String, Arc<Mutex<UnixStream>>> = BTreeMap::new();
    let mut active_connections: BTreeMap<String, u64> = BTreeMap::new();
    let mut fdb: HashMap<[u8; 6], String> = HashMap::new();
    while !shutdown.load(Ordering::SeqCst) {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(SwitchEvent::Attached {
                port,
                connection,
                writer,
            }) => {
                ports.insert(port.clone(), writer);
                active_connections.insert(port.clone(), connection);
                // A forked clone inherits the golden's MAC and reconnects to
                // this same world port while that frozen golden may still hold
                // its old stream open. Flush the old attachment's learned
                // addresses before admitting the replacement.
                fdb.retain(|_, learned_port| learned_port != &port);
            }
            Ok(SwitchEvent::Detached { port, connection }) => {
                if active_connections.get(&port) == Some(&connection) {
                    detach_port(&port, &mut ports, &mut active_connections, &mut fdb);
                }
            }
            Ok(SwitchEvent::Frame {
                port,
                connection,
                frame,
            }) => {
                if active_connections.get(&port) != Some(&connection) || frame.len() < 14 {
                    continue;
                }
                let mut destination = [0; 6];
                destination.copy_from_slice(&frame[..6]);
                let mut source = [0; 6];
                source.copy_from_slice(&frame[6..12]);
                if source[0] & 1 == 0 {
                    fdb.insert(source, port.clone());
                }
                let targets = forwarding_targets(&port, destination, &ports, &fdb, gateway.mac);
                for target in targets {
                    if !write_frame(&target, &frame, &ports) {
                        detach_port(&target, &mut ports, &mut active_connections, &mut fdb);
                    }
                }
                if destination == gateway.mac || should_offer_gateway(destination, &fdb) {
                    if let Some(reply) = gateway.handle(&frame) {
                        let mut reply_destination = [0; 6];
                        reply_destination.copy_from_slice(&reply[..6]);
                        if let Some(target) = fdb.get(&reply_destination).cloned() {
                            if !write_frame(&target, &reply, &ports) {
                                detach_port(&target, &mut ports, &mut active_connections, &mut fdb);
                            }
                        }
                    }
                }
            }
            Ok(SwitchEvent::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

pub(crate) fn should_offer_gateway(destination: [u8; 6], fdb: &HashMap<[u8; 6], String>) -> bool {
    destination == [0xff; 6] || destination[0] & 1 != 0 || !fdb.contains_key(&destination)
}

pub(crate) fn forwarding_targets(
    ingress: &str,
    destination: [u8; 6],
    ports: &BTreeMap<String, Arc<Mutex<UnixStream>>>,
    fdb: &HashMap<[u8; 6], String>,
    gateway_mac: [u8; 6],
) -> Vec<String> {
    if destination == gateway_mac {
        return Vec::new();
    }
    if destination[0] & 1 == 0 {
        if let Some(port) = fdb.get(&destination) {
            return if port != ingress {
                vec![port.clone()]
            } else {
                Vec::new()
            };
        }
    }
    ports
        .keys()
        .filter(|port| port.as_str() != ingress)
        .cloned()
        .collect()
}

pub(crate) fn write_frame(
    port: &str,
    frame: &[u8],
    ports: &BTreeMap<String, Arc<Mutex<UnixStream>>>,
) -> bool {
    let Some(writer) = ports.get(port) else {
        return false;
    };
    let Ok(mut writer) = writer.lock() else {
        return false;
    };
    let length = match u32::try_from(frame.len()) {
        Ok(length) => length.to_be_bytes(),
        Err(_) => return false,
    };
    writer.write_all(&length).is_ok() && writer.write_all(frame).is_ok()
}

pub(crate) fn detach_port(
    port: &str,
    ports: &mut BTreeMap<String, Arc<Mutex<UnixStream>>>,
    active_connections: &mut BTreeMap<String, u64>,
    fdb: &mut HashMap<[u8; 6], String>,
) {
    ports.remove(port);
    active_connections.remove(port);
    fdb.retain(|_, learned_port| learned_port != port);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_known_unicast_and_floods_unknown() {
        let ports = BTreeMap::from([
            (
                "a".into(),
                Arc::new(Mutex::new(UnixStream::pair().unwrap().0)),
            ),
            (
                "b".into(),
                Arc::new(Mutex::new(UnixStream::pair().unwrap().0)),
            ),
            (
                "c".into(),
                Arc::new(Mutex::new(UnixStream::pair().unwrap().0)),
            ),
        ]);
        let mac_b = [2, 0, 0, 0, 0, 2];
        let mut fdb = HashMap::new();
        fdb.insert(mac_b, "b".into());
        assert_eq!(
            forwarding_targets("a", mac_b, &ports, &fdb, crate::model::gateway_mac()),
            ["b"]
        );
        assert_eq!(
            forwarding_targets(
                "a",
                [2, 0, 0, 0, 0, 99],
                &ports,
                &fdb,
                crate::model::gateway_mac(),
            ),
            ["b", "c"]
        );
        assert!(forwarding_targets(
            "a",
            crate::model::gateway_mac(),
            &ports,
            &fdb,
            crate::model::gateway_mac(),
        )
        .is_empty());
    }

    #[test]
    fn accepted_port_stream_blocks_until_a_frame_is_available() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        prepare_port_stream(&reader).unwrap();
        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            writer.write_all(&[42]).unwrap();
        });
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        sender.join().unwrap();
        assert_eq!(byte, [42]);
    }

    #[test]
    fn port_acceptor_replaces_an_active_frozen_nic_connection() {
        // Darwin limits Unix-domain socket paths to 104 bytes, while
        // `temp_dir()` can be a deep per-user `/var/folders/...` path.
        let socket = PathBuf::from("/tmp").join(format!(
            "smw-r-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (switch_tx, switch_rx) = mpsc::channel();
        let (attached_tx, attached_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = spawn_port_acceptor(
            "runner".into(),
            listener,
            switch_tx,
            attached_tx,
            shutdown.clone(),
        )
        .unwrap();

        let first = UnixStream::connect(&socket).unwrap();
        assert_eq!(
            attached_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "runner"
        );
        let first_connection = match switch_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            SwitchEvent::Attached {
                port, connection, ..
            } if port == "runner" => connection,
            _ => panic!("expected first runner attachment"),
        };

        // A fork freezes the golden without necessarily closing its old host
        // stream. The restored clone must therefore be accepted before `first`
        // is dropped.
        let second = UnixStream::connect(&socket).unwrap();
        assert_eq!(
            attached_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "runner"
        );
        let second_connection = match switch_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            SwitchEvent::Attached {
                port, connection, ..
            } if port == "runner" => connection,
            _ => panic!("expected replacement runner attachment"),
        };
        assert_ne!(first_connection, second_connection);

        drop(first);
        assert!(matches!(
            switch_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SwitchEvent::Detached { port, connection }
                if port == "runner" && connection == first_connection
        ));
        drop(second);
        assert!(matches!(
            switch_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SwitchEvent::Detached { port, connection }
                if port == "runner" && connection == second_connection
        ));

        shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }
}
