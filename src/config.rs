use crate::model::{gateway_ip, MachineConfig, MachineResources, NetworkConfig, WorldConfig};
use crate::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use yaml_rust2::{Yaml, YamlLoader};

pub(crate) fn load_config(path: &Path) -> Result<WorldConfig> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_config(&text)
}

pub(crate) fn parse_config(input: &str) -> Result<WorldConfig> {
    let documents = YamlLoader::load_from_str(input)
        .map_err(|error| format!(".smolworld YAML parse error: {error}"))?;
    let [document] = documents.as_slice() else {
        return Err(".smolworld must contain exactly one YAML document".into());
    };
    let root = yaml_hash(document, ".smolworld")?;
    reject_unknown(root, &["world", "network", "machines"], ".smolworld")?;

    let world = yaml_hash(required_key(root, "world", ".smolworld")?, "world")?;
    reject_unknown(world, &["name"], "world")?;
    let name = yaml_string(required_key(world, "name", "world")?, "world.name")?;
    validate_label(&name).map_err(|reason| format!("world.name: {reason}"))?;

    let network = yaml_hash(required_key(root, "network", ".smolworld")?, "network")?;
    reject_unknown(network, &["subnet", "gateway", "dns", "domain"], "network")?;
    let subnet_value = yaml_string(
        required_key(network, "subnet", "network")?,
        "network.subnet",
    )?;
    let subnet = parse_subnet(&subnet_value)?;
    let gateway = optional_key(network, "gateway")
        .map(|value| yaml_ipv4(value, "network.gateway"))
        .transpose()?
        .unwrap_or_else(|| gateway_ip(subnet));
    let dns = optional_key(network, "dns")
        .map(|value| yaml_ipv4(value, "network.dns"))
        .transpose()?
        .unwrap_or(gateway);
    validate_network_identity(subnet, gateway, dns)?;
    let domain = optional_key(network, "domain")
        .map(|value| yaml_string(value, "network.domain"))
        .transpose()?
        .unwrap_or_else(|| name.clone());
    validate_domain(&domain).map_err(|reason| format!("network.domain: {reason}"))?;

    let machine_values = yaml_hash(required_key(root, "machines", ".smolworld")?, "machines")?;
    if machine_values.is_empty() {
        return Err("machines must contain at least one machine".into());
    }
    let mut machines = BTreeMap::new();
    for (machine_key, machine_value) in machine_values {
        let machine_name = match machine_key {
            Yaml::String(value) => value.clone(),
            _ => return Err("machines keys must be strings".into()),
        };
        validate_label(&machine_name)
            .map_err(|reason| format!("machines.{machine_name}: {reason}"))?;
        let path = format!("machines.{machine_name}");
        let machine = yaml_hash(machine_value, &path)?;
        reject_unknown(
            machine,
            &[
                "image",
                "command",
                "depends_on",
                "cpus",
                "memory_mib",
                "storage_gib",
                "overlay_gib",
            ],
            &path,
        )?;
        let image = yaml_string(
            required_key(machine, "image", &path)?,
            &format!("{path}.image"),
        )?;
        if image.is_empty() {
            return Err(format!("{path}.image must not be empty"));
        }
        let command = optional_key(machine, "command")
            .map(|value| yaml_string_array(value, &format!("{path}.command")))
            .transpose()?
            .unwrap_or_default();
        let depends_on = optional_key(machine, "depends_on")
            .map(|value| yaml_string_array(value, &format!("{path}.depends_on")))
            .transpose()?
            .unwrap_or_default();
        let mut unique_dependencies = HashSet::new();
        for dependency in &depends_on {
            validate_label(dependency).map_err(|reason| format!("{path}.depends_on: {reason}"))?;
            if !unique_dependencies.insert(dependency) {
                return Err(format!("{path}.depends_on repeats '{dependency}'"));
            }
        }
        let defaults = MachineResources::default();
        let resources = MachineResources {
            cpus: optional_key(machine, "cpus")
                .map(|value| yaml_positive_integer(value, &format!("{path}.cpus")))
                .transpose()?
                .unwrap_or(defaults.cpus),
            memory_mib: optional_key(machine, "memory_mib")
                .map(|value| yaml_positive_integer(value, &format!("{path}.memory_mib")))
                .transpose()?
                .unwrap_or(defaults.memory_mib),
            storage_gib: optional_key(machine, "storage_gib")
                .map(|value| yaml_positive_integer(value, &format!("{path}.storage_gib")))
                .transpose()?
                .unwrap_or(defaults.storage_gib),
            overlay_gib: optional_key(machine, "overlay_gib")
                .map(|value| yaml_positive_integer(value, &format!("{path}.overlay_gib")))
                .transpose()?
                .unwrap_or(defaults.overlay_gib),
        };
        validate_resources(resources).map_err(|reason| format!("{path}: {reason}"))?;
        if machines
            .insert(
                machine_name.clone(),
                MachineConfig {
                    image,
                    command,
                    depends_on,
                    resources,
                },
            )
            .is_some()
        {
            return Err(format!("machines repeats '{machine_name}'"));
        }
    }
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

fn yaml_hash<'a>(value: &'a Yaml, path: &str) -> Result<&'a yaml_rust2::yaml::Hash> {
    value
        .as_hash()
        .ok_or_else(|| format!("{path} must be a mapping"))
}

fn required_key<'a>(hash: &'a yaml_rust2::yaml::Hash, key: &str, path: &str) -> Result<&'a Yaml> {
    hash.get(&Yaml::String(key.to_string()))
        .ok_or_else(|| format!("{path}.{key} is required"))
}

fn optional_key<'a>(hash: &'a yaml_rust2::yaml::Hash, key: &str) -> Option<&'a Yaml> {
    hash.get(&Yaml::String(key.to_string()))
}

fn reject_unknown(hash: &yaml_rust2::yaml::Hash, allowed: &[&str], path: &str) -> Result<()> {
    for key in hash.keys() {
        let Yaml::String(key) = key else {
            return Err(format!("{path} keys must be strings"));
        };
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{path} contains unsupported key '{key}'"));
        }
    }
    Ok(())
}

fn yaml_string(value: &Yaml, path: &str) -> Result<String> {
    match value {
        Yaml::String(value) => Ok(value.clone()),
        _ => Err(format!("{path} must be a string")),
    }
}

fn yaml_string_array(value: &Yaml, path: &str) -> Result<Vec<String>> {
    let Yaml::Array(values) = value else {
        return Err(format!("{path} must be an array of strings"));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| yaml_string(value, &format!("{path}[{index}]")))
        .collect()
}

fn yaml_ipv4(value: &Yaml, path: &str) -> Result<Ipv4Addr> {
    let value = yaml_string(value, path)?;
    value
        .parse()
        .map_err(|_| format!("{path} must be a valid IPv4 address"))
}

fn yaml_positive_integer<T>(value: &Yaml, path: &str) -> Result<T>
where
    T: TryFrom<i64>,
{
    let Yaml::Integer(value) = value else {
        return Err(format!("{path} must be a positive integer"));
    };
    if *value <= 0 {
        return Err(format!("{path} must be greater than zero"));
    }
    T::try_from(*value).map_err(|_| format!("{path} is out of range"))
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
world:
  name: demo
network:
  subnet: 10.89.0.0/24
machines:
  redis:
    image: ./redis.tar
  client:
    image: ./redis.tar
    command: [sleep, infinity]
    depends_on: [redis]
"#,
        )
        .unwrap()
    }

    #[test]
    fn is_strict_and_orders_dependencies() {
        let config = config();
        assert_eq!(topological_order(&config).unwrap(), ["redis", "client"]);
        assert!(parse_config("world:\n  name: demo\n").is_err());
        assert!(parse_config(
            "world:\n  name: demo\n  unknown: x\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    image: ./x.tar"
        )
        .is_err());
        assert!(parse_config(
            "world:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    image: ./x.tar\n---\nworld: {}"
        )
        .is_err());
        assert!(parse_config(
            "world:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    image: ./x.tar\n    image: ./other.tar"
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
world:
  name: lab
network:
  subnet: 10.97.4.0/24
  gateway: 10.97.4.9
  dns: 10.97.4.9
  domain: lab.test
machines:
  api:
    image: ./api.tar
    cpus: 2
    memory_mib: 512
    storage_gib: 3
    overlay_gib: 2
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
            "world:\n  name: lab\nnetwork:\n  subnet: 10.97.4.0/24\n  dns: 10.97.4.2\nmachines:\n  a:\n    image: ./a.tar"
        )
        .is_err());
        assert!(parse_config(
            "world:\n  name: lab\nnetwork:\n  subnet: 10.97.4.0/24\nmachines:\n  a:\n    image: ./a.tar\n    memory_mib: 63"
        )
        .is_err());
    }
}
