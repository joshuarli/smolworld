use crate::model::{gateway_ip, MachineConfig, MachineResources, NetworkConfig, WorldConfig};
use crate::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;

pub(crate) fn load_config(path: &Path) -> Result<WorldConfig> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_config(&text)
}

pub(crate) fn parse_config(input: &str) -> Result<WorldConfig> {
    #[derive(Clone)]
    enum Section {
        None,
        World,
        Network,
        Machine(String),
    }

    let mut section = Section::None;
    let mut seen = HashSet::new();
    let mut world_name = None;
    let mut subnet = None;
    let mut gateway = None;
    let mut dns = None;
    let mut domain = None;
    let mut machines: BTreeMap<String, MachineConfigBuilder> = BTreeMap::new();

    for (line_number, raw_line) in input.lines().enumerate() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if !line.ends_with(']') || line.starts_with("[[") {
                return Err(config_error(line_number, "expected a single table header"));
            }
            let header = &line[1..line.len() - 1];
            section = match header {
                "world" => Section::World,
                "network" => Section::Network,
                _ => {
                    let Some(name) = header.strip_prefix("machines.") else {
                        return Err(config_error(
                            line_number,
                            format!("unsupported table [{header}]"),
                        ));
                    };
                    validate_label(name).map_err(|reason| config_error(line_number, reason))?;
                    if machines.contains_key(name) {
                        return Err(config_error(
                            line_number,
                            format!("duplicate machine '{name}'"),
                        ));
                    }
                    machines.insert(name.to_string(), MachineConfigBuilder::default());
                    Section::Machine(name.to_string())
                }
            };
            continue;
        }

        let (key, value) = split_assignment(line)
            .ok_or_else(|| config_error(line_number, "expected KEY = VALUE"))?;
        let section_name = match &section {
            Section::None => {
                return Err(config_error(
                    line_number,
                    "key appears before a table header",
                ))
            }
            Section::World => "world".to_string(),
            Section::Network => "network".to_string(),
            Section::Machine(name) => format!("machines.{name}"),
        };
        if !seen.insert(format!("{section_name}.{key}")) {
            return Err(config_error(line_number, format!("duplicate key '{key}'")));
        }

        match &section {
            Section::World if key == "name" => {
                world_name =
                    Some(parse_string(value).map_err(|reason| config_error(line_number, reason))?);
            }
            Section::Network if key == "subnet" => {
                subnet =
                    Some(parse_string(value).map_err(|reason| config_error(line_number, reason))?);
            }
            Section::Network if key == "gateway" => {
                gateway =
                    Some(parse_ipv4(value).map_err(|reason| config_error(line_number, reason))?);
            }
            Section::Network if key == "dns" => {
                dns = Some(parse_ipv4(value).map_err(|reason| config_error(line_number, reason))?);
            }
            Section::Network if key == "domain" => {
                domain =
                    Some(parse_string(value).map_err(|reason| config_error(line_number, reason))?);
            }
            Section::Machine(name) => {
                let machine = machines
                    .get_mut(name)
                    .expect("machine section inserted above");
                match key {
                    "image" => {
                        machine.image = Some(
                            parse_string(value)
                                .map_err(|reason| config_error(line_number, reason))?,
                        )
                    }
                    "command" => {
                        machine.command = Some(
                            parse_string_array(value)
                                .map_err(|reason| config_error(line_number, reason))?,
                        )
                    }
                    "depends_on" => {
                        machine.depends_on = Some(
                            parse_string_array(value)
                                .map_err(|reason| config_error(line_number, reason))?,
                        )
                    }
                    "cpus" => {
                        machine.cpus = Some(
                            parse_positive_integer(value)
                                .map_err(|reason| config_error(line_number, reason))?,
                        )
                    }
                    "memory_mib" => {
                        machine.memory_mib = Some(
                            parse_positive_integer(value)
                                .map_err(|reason| config_error(line_number, reason))?,
                        )
                    }
                    "storage_gib" => {
                        machine.storage_gib = Some(
                            parse_positive_integer(value)
                                .map_err(|reason| config_error(line_number, reason))?,
                        )
                    }
                    "overlay_gib" => {
                        machine.overlay_gib = Some(
                            parse_positive_integer(value)
                                .map_err(|reason| config_error(line_number, reason))?,
                        )
                    }
                    _ => {
                        return Err(config_error(
                            line_number,
                            format!("unsupported machine key '{key}'"),
                        ))
                    }
                }
            }
            Section::World | Section::Network => {
                return Err(config_error(
                    line_number,
                    format!("unsupported key '{key}'"),
                ))
            }
            Section::None => unreachable!(),
        }
    }

    let name = world_name.ok_or_else(|| "missing [world].name".to_string())?;
    validate_label(&name).map_err(|reason| format!("[world].name: {reason}"))?;
    let subnet = parse_subnet(&subnet.ok_or_else(|| "missing [network].subnet".to_string())?)?;
    let gateway = gateway.unwrap_or_else(|| gateway_ip(subnet));
    let dns = dns.unwrap_or(gateway);
    validate_network_identity(subnet, gateway, dns)?;
    let domain = domain.unwrap_or_else(|| name.clone());
    validate_domain(&domain).map_err(|reason| format!("[network].domain: {reason}"))?;
    if machines.is_empty() {
        return Err("at least one [machines.NAME] table is required".into());
    }
    let machines = machines
        .into_iter()
        .map(|(name, builder)| {
            let image = builder
                .image
                .ok_or_else(|| format!("[machines.{name}].image is required"))?;
            if image.is_empty() {
                return Err(format!("[machines.{name}].image must not be empty"));
            }
            let depends_on = builder.depends_on.unwrap_or_default();
            let mut unique_dependencies = HashSet::new();
            for dependency in &depends_on {
                validate_label(dependency)
                    .map_err(|reason| format!("[machines.{name}].depends_on: {reason}"))?;
                if !unique_dependencies.insert(dependency) {
                    return Err(format!(
                        "[machines.{name}].depends_on repeats '{dependency}'"
                    ));
                }
            }
            let defaults = MachineResources::default();
            let resources = MachineResources {
                cpus: builder.cpus.unwrap_or(defaults.cpus),
                memory_mib: builder.memory_mib.unwrap_or(defaults.memory_mib),
                storage_gib: builder.storage_gib.unwrap_or(defaults.storage_gib),
                overlay_gib: builder.overlay_gib.unwrap_or(defaults.overlay_gib),
            };
            validate_resources(resources)
                .map_err(|reason| format!("[machines.{name}]: {reason}"))?;
            Ok((
                name,
                MachineConfig {
                    image,
                    command: builder.command.unwrap_or_default(),
                    depends_on,
                    resources,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let config = WorldConfig {
        name,
        network: NetworkConfig {
            subnet,
            gateway,
            dns,
            domain,
        },
        machines,
    };
    topological_order(&config)?;
    Ok(config)
}

#[derive(Default)]
pub(crate) struct MachineConfigBuilder {
    image: Option<String>,
    command: Option<Vec<String>>,
    depends_on: Option<Vec<String>>,
    cpus: Option<u8>,
    memory_mib: Option<u32>,
    storage_gib: Option<u64>,
    overlay_gib: Option<u64>,
}

pub(crate) fn config_error(line_number: usize, reason: impl std::fmt::Display) -> String {
    format!(".smolworld line {}: {reason}", line_number + 1)
}

pub(crate) fn strip_comment(value: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == '#' && !quoted {
            return &value[..index];
        }
    }
    value
}

pub(crate) fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == '=' && !quoted {
            let key = line[..index].trim();
            let value = line[index + 1..].trim();
            if !key.is_empty() && !value.is_empty() {
                return Some((key, value));
            }
            return None;
        }
    }
    None
}

pub(crate) fn parse_string(value: &str) -> Result<String> {
    let (parsed, rest) = parse_string_prefix(value)?;
    if rest.trim().is_empty() {
        Ok(parsed)
    } else {
        Err("unexpected text after string".into())
    }
}

pub(crate) fn parse_string_prefix(value: &str) -> Result<(String, &str)> {
    let value = value.trim_start();
    let Some(rest) = value.strip_prefix('"') else {
        return Err("expected a double-quoted string".into());
    };
    let mut result = String::new();
    let mut escaped = false;
    for (index, character) in rest.char_indices() {
        if escaped {
            match character {
                '"' | '\\' => result.push(character),
                'n' => result.push('\n'),
                _ => return Err("unsupported string escape".into()),
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return Ok((result, &rest[index + character.len_utf8()..]));
        } else {
            result.push(character);
        }
    }
    Err("unterminated string".into())
}

pub(crate) fn parse_string_array(value: &str) -> Result<Vec<String>> {
    let mut rest = value.trim_start();
    let Some(after_open) = rest.strip_prefix('[') else {
        return Err("expected an array of double-quoted strings".into());
    };
    rest = after_open.trim_start();
    let mut result = Vec::new();
    if let Some(after_close) = rest.strip_prefix(']') {
        if after_close.trim().is_empty() {
            return Ok(result);
        }
        return Err("unexpected text after array".into());
    }
    loop {
        let (item, after_item) = parse_string_prefix(rest)?;
        result.push(item);
        rest = after_item.trim_start();
        if let Some(after_close) = rest.strip_prefix(']') {
            if after_close.trim().is_empty() {
                return Ok(result);
            }
            return Err("unexpected text after array".into());
        }
        let Some(after_comma) = rest.strip_prefix(',') else {
            return Err("array items must be separated by commas".into());
        };
        rest = after_comma.trim_start();
    }
}

pub(crate) fn validate_label(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 63 {
        return Err("must be a 1–63 character lowercase DNS label".into());
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
        || !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit()
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && *byte != b'-')
    {
        return Err("must be a lowercase RFC-1123-style DNS label".into());
    }
    Ok(())
}

pub(crate) fn parse_subnet(value: &str) -> Result<[u8; 4]> {
    let (address, prefix) = value
        .split_once('/')
        .ok_or_else(|| "[network].subnet must be IPv4/24".to_string())?;
    if prefix != "24" {
        return Err("[network].subnet must use /24 in this PoC".into());
    }
    let address = address
        .parse::<Ipv4Addr>()
        .map_err(|_| "[network].subnet must contain a valid IPv4 address".to_string())?;
    let octets = address.octets();
    if octets[3] != 0 {
        return Err("[network].subnet must be the /24 network address ending in .0".into());
    }
    Ok(octets)
}

pub(crate) fn parse_ipv4(value: &str) -> Result<Ipv4Addr> {
    let value = parse_string(value)?;
    value
        .parse()
        .map_err(|_| "expected a valid IPv4 address".to_string())
}

pub(crate) fn parse_positive_integer<T>(value: &str) -> Result<T>
where
    T: std::str::FromStr + PartialEq + From<u8>,
{
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected a positive integer".into());
    }
    let parsed = value
        .parse::<T>()
        .map_err(|_| "integer is out of range".to_string())?;
    if parsed == T::from(0) {
        return Err("must be greater than zero".into());
    }
    Ok(parsed)
}

pub(crate) fn validate_network_identity(
    subnet: [u8; 4],
    gateway: Ipv4Addr,
    dns: Ipv4Addr,
) -> Result<()> {
    let in_subnet = |address: Ipv4Addr| {
        let octets = address.octets();
        octets[..3] == subnet[..3] && (1..=254).contains(&octets[3])
    };
    if !in_subnet(gateway) {
        return Err("[network].gateway must be a usable address in [network].subnet".into());
    }
    if dns != gateway {
        return Err(
            "[network].dns must equal [network].gateway: this PoC only provides its synthetic authoritative DNS service"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn validate_domain(domain: &str) -> Result<()> {
    if domain.is_empty() || domain.len() > 253 {
        return Err("must be a non-empty DNS domain".into());
    }
    for label in domain.split('.') {
        validate_label(label)
            .map_err(|_| "must contain lowercase DNS labels separated by dots".to_string())?;
    }
    Ok(())
}

pub(crate) fn validate_resources(resources: MachineResources) -> Result<()> {
    if resources.memory_mib < 64 {
        return Err("memory_mib must be at least 64".into());
    }
    Ok(())
}

pub(crate) fn topological_order(config: &WorldConfig) -> Result<Vec<String>> {
    fn visit(
        name: &str,
        config: &WorldConfig,
        marks: &mut HashMap<String, u8>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        match marks.get(name).copied().unwrap_or(0) {
            1 => return Err(format!("depends_on contains a cycle at '{name}'")),
            2 => return Ok(()),
            _ => {}
        }
        let machine = config
            .machines
            .get(name)
            .ok_or_else(|| format!("depends_on references unknown machine '{name}'"))?;
        marks.insert(name.to_string(), 1);
        for dependency in &machine.depends_on {
            if !config.machines.contains_key(dependency) {
                return Err(format!(
                    "machine '{name}' depends_on unknown machine '{dependency}'"
                ));
            }
            visit(dependency, config, marks, order)?;
        }
        marks.insert(name.to_string(), 2);
        order.push(name.to_string());
        Ok(())
    }

    let mut marks = HashMap::new();
    let mut order = Vec::new();
    for name in config.machines.keys() {
        visit(name, config, &mut marks, &mut order)?;
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WorldConfig {
        parse_config(
            r#"
[world]
name = "demo"

[network]
subnet = "10.89.0.0/24"

[machines.redis]
image = "./redis.tar"

[machines.client]
image = "./redis.tar"
command = ["sleep", "infinity"]
depends_on = ["redis"]
"#,
        )
        .unwrap()
    }

    #[test]
    fn is_strict_and_orders_dependencies() {
        let config = config();
        assert_eq!(topological_order(&config).unwrap(), ["redis", "client"]);
        assert!(parse_config("[world]\nname = \"demo\"\n").is_err());
        assert!(parse_config(
            "[world]\nname=\"demo\"\nunknown=\"x\"\n[network]\nsubnet=\"10.89.0.0/24\"\n[machines.a]\nimage=\"./x.tar\""
        )
        .is_err());
    }

    #[test]
    fn rejects_non_24_and_dependency_cycle() {
        assert!(parse_subnet("10.89.0.0/16").is_err());
        let mut cyclic = config();
        cyclic
            .machines
            .get_mut("redis")
            .unwrap()
            .depends_on
            .push("client".into());
        assert!(topological_order(&cyclic).unwrap_err().contains("cycle"));
    }

    #[test]
    fn accepts_generic_network_and_machine_resources() {
        let config = parse_config(
            r#"
[world]
name = "lab"

[network]
subnet = "10.97.4.0/24"
gateway = "10.97.4.9"
dns = "10.97.4.9"
domain = "lab.test"

[machines.api]
image = "./api.tar"
cpus = 2
memory_mib = 512
storage_gib = 3
overlay_gib = 2
"#,
        )
        .unwrap();
        assert_eq!(config.network.gateway, Ipv4Addr::new(10, 97, 4, 9));
        assert_eq!(config.network.dns, config.network.gateway);
        assert_eq!(config.network.domain, "lab.test");
        assert_eq!(
            config.machines["api"].resources,
            MachineResources {
                cpus: 2,
                memory_mib: 512,
                storage_gib: 3,
                overlay_gib: 2,
            }
        );
        assert!(parse_config(
            "[world]\nname=\"lab\"\n[network]\nsubnet=\"10.97.4.0/24\"\ndns=\"10.97.4.2\"\n[machines.a]\nimage=\"./a.tar\""
        )
        .is_err());
        assert!(parse_config(
            "[world]\nname=\"lab\"\n[network]\nsubnet=\"10.97.4.0/24\"\n[machines.a]\nimage=\"./a.tar\"\nmemory_mib=63"
        )
        .is_err());
    }
}
