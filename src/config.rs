use crate::model::{gateway_ip, MachineConfig, NetworkConfig, SeedFile, WorldConfig};
use crate::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
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
    reject_unknown(
        root,
        &["format", "world", "network", "machines"],
        ".smolworld",
    )?;
    let format = required_key(root, "format", ".smolworld")?;
    if !matches!(format, Yaml::Integer(2)) {
        return Err(".smolworld.format must be exactly 2".into());
    }

    let world = yaml_hash(required_key(root, "world", ".smolworld")?, "world")?;
    reject_unknown(world, &["name"], "world")?;
    let name = yaml_string(required_key(world, "name", "world")?, "world.name")?;
    validate_label(&name).map_err(|reason| format!("world.name: {reason}"))?;

    let network = yaml_hash(required_key(root, "network", ".smolworld")?, "network")?;
    reject_unknown(
        network,
        &["subnet", "gateway", "dns", "domain", "egress"],
        "network",
    )?;
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
    let egress = optional_key(network, "egress")
        .map(|value| yaml_bool(value, "network.egress"))
        .transpose()?
        .unwrap_or(false);

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
        reject_unknown(machine, &["smolfile", "depends_on", "seed_files"], &path)?;
        let smolfile = yaml_world_relative_path(
            required_key(machine, "smolfile", &path)?,
            &format!("{path}.smolfile"),
        )?;
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
        let seed_files = optional_key(machine, "seed_files")
            .map(|value| parse_seed_files(value, &format!("{path}.seed_files")))
            .transpose()?
            .unwrap_or_default();
        if machines
            .insert(
                machine_name.clone(),
                MachineConfig {
                    smolfile,
                    depends_on,
                    seed_files,
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
            egress,
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

fn yaml_path(value: &Yaml, path: &str) -> Result<PathBuf> {
    let value = yaml_string(value, path)?;
    if value.is_empty() {
        return Err(format!("{path} must not be empty"));
    }
    Ok(PathBuf::from(value))
}

/// Authored world material must stay inside the world directory. A prepared
/// world is copied into an immutable run snapshot before it is run, so an
/// absolute host path or lexical escape would make its material lock
/// non-portable and would let a snapshot silently consume an undeclared host
/// input.
fn yaml_world_relative_path(value: &Yaml, path: &str) -> Result<PathBuf> {
    let value = yaml_path(value, path)?;
    if value.is_absolute()
        || value.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "{path} must be a non-escaping path relative to the .smolworld file"
        ));
    }
    Ok(value)
}

fn parse_seed_files(value: &Yaml, path: &str) -> Result<Vec<SeedFile>> {
    let Yaml::Array(values) = value else {
        return Err(format!("{path} must be an array of mappings"));
    };
    let mut seed_files = Vec::with_capacity(values.len());
    let mut destinations = HashSet::new();
    for (index, value) in values.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let item = yaml_hash(value, &item_path)?;
        reject_unknown(item, &["source", "destination", "mode"], &item_path)?;
        let source = yaml_world_relative_path(
            required_key(item, "source", &item_path)?,
            &format!("{item_path}.source"),
        )?;
        let destination = yaml_path(
            required_key(item, "destination", &item_path)?,
            &format!("{item_path}.destination"),
        )?;
        if !destination.is_absolute() {
            return Err(format!(
                "{item_path}.destination must be an absolute guest path"
            ));
        }
        if !destinations.insert(destination.clone()) {
            return Err(format!(
                "{path} repeats destination '{}'",
                destination.display()
            ));
        }
        let mode = yaml_mode(
            required_key(item, "mode", &item_path)?,
            &format!("{item_path}.mode"),
        )?;
        seed_files.push(SeedFile {
            source,
            destination,
            mode,
        });
    }
    Ok(seed_files)
}

fn yaml_bool(value: &Yaml, path: &str) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| format!("{path} must be true or false"))
}

fn yaml_mode(value: &Yaml, path: &str) -> Result<u32> {
    let value = yaml_string(value, path)?;
    if value.len() != 4 || !value.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return Err(format!(
            "{path} must be a four-digit octal mode such as \"0644\""
        ));
    }
    u32::from_str_radix(&value, 8).map_err(|_| format!("{path} is out of range"))
}

fn yaml_ipv4(value: &Yaml, path: &str) -> Result<Ipv4Addr> {
    let value = yaml_string(value, path)?;
    value
        .parse()
        .map_err(|_| format!("{path} must be a valid IPv4 address"))
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

/// Group machines into deterministic dependency waves. Machines in one wave
/// have no dependency on another machine in that same wave, so host-side
/// preparation and lifecycle operations may run concurrently. The wave
/// boundary preserves the world contract that depends_on controls creation
/// and start order.
pub(crate) fn topological_waves(config: &WorldConfig) -> Result<Vec<Vec<String>>> {
    let order = topological_order(config)?;
    let mut depths = HashMap::new();
    let mut waves: Vec<Vec<String>> = Vec::new();

    for name in order {
        let depth = config
            .machines
            .get(&name)
            .expect("topological order contains configured machines")
            .depends_on
            .iter()
            .map(|dependency| depths[dependency] + 1)
            .max()
            .unwrap_or(0);
        depths.insert(name.clone(), depth);
        if waves.len() <= depth {
            waves.resize_with(depth + 1, Vec::new);
        }
        waves[depth].push(name);
    }
    Ok(waves)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WorldConfig {
        parse_config(
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
    depends_on: [redis]
"#,
        )
        .unwrap()
    }

    #[test]
    fn is_strict_and_orders_dependencies() {
        let config = config();
        assert_eq!(topological_order(&config).unwrap(), ["redis", "client"]);
        assert_eq!(
            topological_waves(&config).unwrap(),
            [["redis".to_string()], ["client".to_string()]]
        );
        assert!(parse_config(
            "world:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    smolfile: ./a.Smolfile"
        )
        .unwrap_err()
        .contains("format"));
        assert!(parse_config(
            "format: 1\nworld:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    smolfile: ./a.Smolfile"
        )
        .unwrap_err()
        .contains("exactly 2"));
        assert!(parse_config(
            "format: 2\nworld:\n  name: demo\n  unknown: x\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    smolfile: ./a.Smolfile"
        )
        .is_err());
        assert!(parse_config(
            "format: 2\nworld:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    smolfile: ./a.Smolfile\n---\nformat: 2\nworld: {}"
        )
        .is_err());
        assert!(parse_config(
            "format: 2\nworld:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    image: ./x.tar"
        )
        .is_err());
        assert!(parse_config(
            "format: 2\nworld:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  a:\n    depends_on: []"
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
    fn groups_independent_machines_into_one_wave() {
        let config = parse_config(
            r#"
format: 2
world:
  name: demo
network:
  subnet: 10.89.0.0/24
machines:
  client:
    smolfile: ./client.Smolfile
    depends_on: [redis]
  redis:
    smolfile: ./redis.Smolfile
  postgres:
    smolfile: ./postgres.Smolfile
"#,
        )
        .unwrap();
        assert_eq!(
            topological_waves(&config).unwrap(),
            vec![
                vec!["redis".to_string(), "postgres".to_string()],
                vec!["client".to_string()]
            ]
        );
    }

    #[test]
    fn accepts_seed_files_and_rejects_legacy_machine_fields() {
        let config = parse_config(
            r#"
format: 2
world:
  name: lab
network:
  subnet: 10.97.4.0/24
  gateway: 10.97.4.9
  dns: 10.97.4.9
  domain: lab.test
machines:
  api:
    smolfile: ./api.Smolfile
    seed_files:
      - source: ./assets/config.xml
        destination: /etc/app/config.xml
        mode: "0644"
      - source: ./assets/secret
        destination: /run/app/secret
        mode: "0600"
"#,
        )
        .unwrap();
        assert_eq!(config.network.gateway, Ipv4Addr::new(10, 97, 4, 9));
        assert_eq!(config.network.dns, config.network.gateway);
        assert_eq!(config.network.domain, "lab.test");
        assert_eq!(
            config.machines["api"].smolfile,
            PathBuf::from("./api.Smolfile")
        );
        assert_eq!(
            config.machines["api"].seed_files[0].source,
            PathBuf::from("./assets/config.xml")
        );
        assert_eq!(
            config.machines["api"].seed_files[0].destination,
            PathBuf::from("/etc/app/config.xml")
        );
        assert_eq!(config.machines["api"].seed_files[0].mode, 0o644);
        assert_eq!(config.machines["api"].seed_files[1].mode, 0o600);
        assert!(parse_config(
            "format: 2\nworld:\n  name: lab\nnetwork:\n  subnet: 10.97.4.0/24\n  dns: 10.97.4.2\nmachines:\n  a:\n    smolfile: ./a.Smolfile"
        )
        .is_err());
        assert!(parse_config(
            "format: 2\nworld:\n  name: lab\nnetwork:\n  subnet: 10.97.4.0/24\nmachines:\n  a:\n    smolfile: ./a.Smolfile\n    memory_mib: 512"
        )
        .is_err());
        assert!(parse_config(
            "format: 2\nworld:\n  name: lab\nnetwork:\n  subnet: 10.97.4.0/24\nmachines:\n  a:\n    smolfile: /tmp/a.Smolfile"
        )
        .unwrap_err()
        .contains("relative"));
    }

    #[test]
    fn parses_explicit_network_egress() {
        let config = parse_config(
            "format: 2\nworld:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\n  egress: true\nmachines:\n  a:\n    smolfile: ./a.Smolfile\n",
        )
        .unwrap();
        assert!(config.network.egress);
    }

    #[test]
    fn rejects_invalid_seed_file_declarations() {
        let base = |seed_file| {
            format!(
                "format: 2\nworld:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  api:\n    smolfile: ./api.Smolfile\n    seed_files:\n{seed_file}"
            )
        };
        assert!(parse_config(&base(
            "      - source: ./config\n        destination: relative\n        mode: \"0644\"\n"
        ))
        .is_err());
        assert!(parse_config(&base(
            "      - source: ./config\n        destination: /etc/config\n        mode: 0644\n"
        ))
        .is_err());
        assert!(parse_config(&base(
            "      - source: ./config\n        destination: /etc/config\n        mode: \"644\"\n"
        ))
        .is_err());
        assert!(parse_config(&base("      - source: ./one\n        destination: /etc/config\n        mode: \"0644\"\n      - source: ./two\n        destination: /etc/config\n        mode: \"0600\"\n")).is_err());
        assert!(parse_config(&base(
            "      - source: ../outside\n        destination: /etc/config\n        mode: \"0644\"\n"
        ))
        .is_err());
    }

    #[test]
    fn rejects_semantically_invalid_network_and_dependency_boundaries() {
        let base = |network: &str, machine: &str| {
            format!(
                "format: 2\nworld:\n  name: demo\nnetwork:\n  subnet: {network}\nmachines:\n  {machine}:\n    smolfile: ./{machine}.Smolfile\n"
            )
        };
        assert!(parse_config(&base("10.89.0.0/24\n  gateway: 10.90.0.1", "api")).is_err());
        assert!(parse_config(&base("10.89.0.0/24\n  domain: Demo.TEST", "api")).is_err());
        assert!(parse_config(&base("10.89.0.0/24", "API")).is_err());
        assert!(parse_config(
            "format: 2\nworld:\n  name: demo\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  api:\n    smolfile: ./api.Smolfile\n    depends_on: [missing]\n"
        )
        .unwrap_err()
        .contains("unknown machine"));
    }
}
