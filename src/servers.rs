use colored::Colorize;
use regex::Regex;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fmt, fs,
    net::IpAddr,
    process::Stdio,
    sync::Arc,
    time::Instant,
};
use tokio::{net::TcpStream, process::Command, sync::RwLock};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServerConfigError {
    #[error("Failed to parse config file: {0}")]
    ParseError(String),

    #[error("Found undefined dependency: {0}")]
    UndefinedDependency(String),

    #[error("Found circular dependency: {0}")]
    CircularDependency(String),

    #[error("Misconfigured healthcheck: {0}")]
    BadHealthCheckDefinition(String),
}

fn default_retry_duration() -> std::time::Duration {
    std::time::Duration::from_secs(10)
}

fn default_timeout_duration() -> std::time::Duration {
    // 5 minutes timeout
    // some servers might take longer, but that can be overridden in the config
    std::time::Duration::from_secs(300)
}

#[derive(Debug, Clone, Copy, Default)]
pub enum CheckStatus {
    #[default]
    Waiting,
    Running,
    TimedOut,
    Ok,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HealthCheck {
    #[serde(default = "default_retry_duration", with = "humantime_serde")]
    pub retry: std::time::Duration,

    #[serde(default = "default_timeout_duration", with = "humantime_serde")]
    pub timeout: std::time::Duration,

    #[serde(flatten)]
    pub method: HealthCheckMethod,

    #[serde(skip)]
    pub status: CheckStatus,
}

impl fmt::Display for HealthCheck {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.method)
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HealthCheckMethod {
    Http {
        url: String,
        status: Option<u16>,
        #[serde(default, with = "serde_regex")]
        regex: Option<Regex>,
    },
    Port {
        ip: String,
        port: u16,
    },
    Shell {
        command: String,
        status: Option<i32>,
        #[serde(default, with = "serde_regex")]
        regex: Option<Regex>,
    },
}

fn truncate_command(command: &str, max_length: usize) -> String {
    if command.len() > max_length {
        // Truncate to 27 characters and add "..." to make it 30 characters in total
        format!("{}{}", &command[..max_length - 3], "...".yellow())
    } else {
        command.to_string()
    }
}

impl fmt::Display for HealthCheckMethod {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            HealthCheckMethod::Http {
                url,
                status: _,
                regex: _,
            } => write!(f, "{} [{}]", "http".bold(), url),
            HealthCheckMethod::Port { ip, port } => {
                write!(f, "{} [{}:{}]", "port".bold(), ip, port)
            }
            HealthCheckMethod::Shell {
                command,
                status: _,
                regex: _,
            } => write!(f, "{} [{}]", "shell".bold(), truncate_command(command, 30)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServerStatus {
    #[default]
    Waiting,
    WOLSent,
    Ok,
    TimedOut,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Server {
    pub name: String,
    pub mac: String,
    pub interface: String,
    #[serde(default)]
    pub vlan: Option<u16>,

    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub check: Vec<HealthCheck>,

    #[serde(skip)]
    pub status: ServerStatus,
}

fn map_server_names(servers: &[Server]) -> HashMap<String, &Server> {
    servers.iter().map(|s| (s.name.clone(), s)).collect()
}

fn determine_wakeup_order(servers: &[Server]) -> Result<Vec<Server>, ServerConfigError> {
    let server_from_name = map_server_names(servers);

    let mut visited = HashSet::new();
    let mut visiting = HashSet::new();
    let mut sorted = Vec::new();

    for server in servers {
        if !visited.contains(&server.name) {
            depth_first_search(
                server,
                &server_from_name,
                &mut visited,
                &mut visiting,
                &mut sorted,
            )?;
        }
    }

    let servers_in_order: Vec<Server> = sorted
        .iter()
        .map(|name| *server_from_name.get(name).unwrap())
        .cloned()
        .collect();
    Ok(servers_in_order)
}

fn depth_first_search(
    server: &Server,
    server_from_name: &HashMap<String, &Server>,
    visited: &mut HashSet<String>,
    visiting: &mut HashSet<String>,
    sorted: &mut Vec<String>,
) -> Result<(), ServerConfigError> {
    if visiting.contains(&server.name) {
        return Err(ServerConfigError::CircularDependency(server.name.clone()));
    }

    if visited.contains(&server.name) {
        return Ok(());
    }

    visiting.insert(server.name.clone());

    for dep in &server.depends {
        let dep_server = server_from_name
            .get(dep)
            .ok_or_else(|| ServerConfigError::UndefinedDependency(dep.clone()))?;
        depth_first_search(dep_server, server_from_name, visited, visiting, sorted)?;
    }

    visiting.remove(&server.name);
    visited.insert(server.name.clone());

    sorted.push(server.name.clone());

    Ok(())
}

fn validate_health_check(healthcheck: &HealthCheckMethod) -> Result<(), ServerConfigError> {
    match healthcheck {
        HealthCheckMethod::Http {
            url: _,
            status,
            regex,
        } => {
            if status.is_none() && regex.is_none() {
                return Err(ServerConfigError::BadHealthCheckDefinition("HTTP health check requires an HTTP status code to match and/or a Regex to match in the response".into()));
            }
        }
        HealthCheckMethod::Port { ip, port: _ } => {
            if ip.parse::<IpAddr>().is_err() {
                return Err(ServerConfigError::BadHealthCheckDefinition(
                    "Port check requires a valid IP address".into(),
                ));
            }
        }
        HealthCheckMethod::Shell {
            command: _,
            status,
            regex,
        } => {
            if status.is_none() && regex.is_none() {
                return Err(ServerConfigError::BadHealthCheckDefinition("Health check via shell command requires an return code to match and/or a Regex to match in the standard output".into()));
            }
        }
    }

    Ok(())
}

pub fn parse_server_dependencies(file_path: &str) -> Result<Vec<Server>, ServerConfigError> {
    let yaml_content =
        fs::read_to_string(file_path).map_err(|e| ServerConfigError::ParseError(e.to_string()))?;

    let servers: Vec<Server> = serde_yaml_ng::from_str(&yaml_content)
        .map_err(|e| ServerConfigError::ParseError(e.to_string()))?;

    for server in &servers {
        for healthcheck in &server.check {
            validate_health_check(&healthcheck.method)?;
        }
    }

    // Apply topological sort to determine order to wake the servers
    // check for circular and undefined servers along the way
    let sorted = determine_wakeup_order(&servers)?;

    Ok(sorted)
}

async fn http_health_check(
    url: &str,
    expected_status: Option<u16>,
    payload_regex: Option<Regex>,
) -> bool {
    if let Ok(response) = reqwest::get(url).await {
        if let Some(status) = expected_status {
            if response.status().as_u16() != status {
                return false;
            }
        }
        if let Some(regex) = payload_regex {
            if let Ok(body) = response.text().await {
                if regex.is_match(&body) {
                    return true;
                }
            };
            return false;
        }
        return true;
    };
    false
}

async fn port_health_check(ip: &str, port: u16) -> bool {
    let address = format!("{}:{}", ip, port);
    TcpStream::connect(address).await.is_ok()
}

async fn shell_health_check(
    command: &str,
    expected_status: Option<i32>,
    payload_regex: Option<Regex>,
) -> bool {
    let result = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    if let Ok(output) = result {
        if let Some(status) = expected_status {
            if output.status.code() != Some(status) {
                return false;
            }
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(regex) = payload_regex {
            if !regex.is_match(&stdout) {
                return false;
            }
        }
        return true;
    };
    false
}

pub async fn check_health(check: HealthCheckMethod) -> bool {
    match check {
        HealthCheckMethod::Http { url, status, regex } => {
            http_health_check(&url, status, regex).await
        }
        HealthCheckMethod::Port { ip, port } => port_health_check(&ip, port).await,
        HealthCheckMethod::Shell {
            command,
            status,
            regex,
        } => shell_health_check(&command, status, regex).await,
    }
}

pub async fn perform_health_checks(
    servers: Arc<RwLock<Vec<Server>>>,
    index: usize,
) -> ServerStatus {
    let mut tasks = Vec::new();

    let checks = {
        let servers_read = servers.read().await;
        servers_read[index].check.clone()
    };

    for (check_index, check) in checks.into_iter().enumerate() {
        {
            let mut servers_write = servers.write().await;
            servers_write[index].check[check_index].status = CheckStatus::Running;
        }

        let servers_clone = servers.clone();
        tasks.push(tokio::spawn(async move {
            let start_time = Instant::now();
            loop {
                if start_time.elapsed() >= check.timeout {
                    {
                        let mut servers_write = servers_clone.write().await;
                        servers_write[index].check[check_index].status = CheckStatus::TimedOut;
                    }
                    return CheckStatus::TimedOut;
                }
                if check_health(check.method.clone()).await {
                    break;
                } else {
                    tokio::time::sleep(check.retry).await;
                }
            }
            {
                let mut servers_write = servers_clone.write().await;
                servers_write[index].check[check_index].status = CheckStatus::Ok;
            }
            CheckStatus::Ok
        }))
    }

    let mut timeout = false;
    for task in tasks {
        if let CheckStatus::TimedOut = task.await.unwrap() {
            timeout = true;
        }
    }
    {
        let mut servers_write = servers.write().await;
        servers_write[index].status = if timeout {
            ServerStatus::TimedOut
        } else {
            ServerStatus::Ok
        };
    }

    if timeout {
        ServerStatus::TimedOut
    } else {
        ServerStatus::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circular_dependencies() {
        let yaml_data = r#"
        - name: "server1"
          mac: "00:11:22:33:44:55"
          interface: "eth0"
          vlan: 100
          depends:
            - "server2"
          check: []

        - name: "server2"
          mac: "66:77:88:99:AA:BB"
          interface: "eth0"
          vlan: 100
          depends:
            - "server3"
          check: []

        - name: "server3"
          mac: "AA:BB:CC:DD:EE:FF"
          interface: "eth1"
          vlan: 200
          depends:
            - "server4"
          check: []

        - name: "server4"
          mac: "FF:EE:DD:CC:BB:AA"
          interface: "eth1"
          vlan: 200
          depends:
            - "server1"
          check: []
        "#;

        let servers: Vec<Server> =
            serde_yaml_ng::from_str(yaml_data).expect("Failed to parse YAML");

        let result = determine_wakeup_order(&servers);
        match result {
            Err(ServerConfigError::CircularDependency(circular_server)) => {
                assert_eq!(circular_server, "server1".to_string());
            }
            _ => panic!("Expected a circular dependency error"),
        }
    }

    #[test]
    fn test_no_circular_dependencies_with() {
        let yaml_data = r#"
        - name: "server1"
          mac: "00:11:22:33:44:55"
          interface: "eth0"
          vlan: 100
          depends:
            - "server2"
          check: []

        - name: "server2"
          mac: "66:77:88:99:AA:BB"
          interface: "eth0"
          vlan: 100
          depends:
            - "server3"
          check: []

        - name: "server3"
          mac: "AA:BB:CC:DD:EE:FF"
          interface: "eth1"
          vlan: 200
          depends: []
          check: []
        "#;

        // Deserialize the YAML string into a ServerDependencyConfig
        let servers: Vec<Server> =
            serde_yaml_ng::from_str(yaml_data).expect("Failed to parse YAML");

        // Call the validate_dependencies function and check if it passes without errors.
        let result = determine_wakeup_order(&servers);

        // We expect no errors, meaning no circular dependencies exist.
        assert!(result.is_ok(), "Expected no circular dependencies");
    }

    #[test]
    fn test_invalid_http_check() {
        let yaml_data = r#"
        name: "server1"
        mac: "00:11:22:33:44:55"
        interface: "eth0"
        vlan: 100
        depends: []
        check:
          - type: http
            url: "http://example.com"
        "#;

        let server: Server = serde_yaml_ng::from_str(yaml_data).expect("Failed to parse YAML");
        let result = validate_health_check(&server.check[0].method);
        assert!(matches!(
            result,
            Err(ServerConfigError::BadHealthCheckDefinition(_))
        ));
    }

    #[test]
    fn test_invalid_shell_check() {
        let yaml_data = r#"
        name: "server1"
        mac: "00:11:22:33:44:55"
        interface: "eth0"
        vlan: 100
        depends: []
        check:
          - type: shell
            command: curl something 
            retry: 2 minutes
        "#;

        let server: Server = serde_yaml_ng::from_str(yaml_data).expect("Failed to parse YAML");
        let result = validate_health_check(&server.check[0].method);
        assert!(matches!(
            result,
            Err(ServerConfigError::BadHealthCheckDefinition(_))
        ));
    }

    #[test]
    fn test_invalid_port_check() {
        let yaml_data = r#"
        name: "server1"
        mac: "00:11:22:33:44:55"
        interface: "eth0"
        vlan: 100
        depends: []
        check:
          - type: port
            ip: "invalid_ip"   # Invalid IP address
            port: 80
        "#;

        let server: Server = serde_yaml_ng::from_str(yaml_data).expect("Failed to parse YAML");
        let result = validate_health_check(&server.check[0].method);

        assert!(matches!(
            result,
            Err(ServerConfigError::BadHealthCheckDefinition(_))
        ));
    }

    #[test]
    fn test_valid_health_checks() {
        let yaml_data = r#"
        name: "server1"
        mac: "00:11:22:33:44:55"
        interface: "eth0"
        vlan: 100
        depends: []
        check:
          - type: http
            url: "http://example.com"
            status: 200          # Valid: status is provided
            regex: ~
          - type: port
            ip: "192.168.1.1"    # Valid IP
            port: 80
          - type: shell
            command: "echo Hello"
            status: ~            # Valid: regex is provided
            regex: "Hello"
        "#;

        let server: Server = serde_yaml_ng::from_str(yaml_data).expect("Failed to parse YAML");
        for healthcheck in &server.check {
            let result = validate_health_check(&healthcheck.method);
            assert!(result.is_ok())
        }
    }

    #[test]
    fn test_determine_wakeup_order() {
        // Define the YAML string for servers with dependencies
        let yaml_data = r#"
        - name: "server_a"
          mac: "00:11:22:33:44:55"
          interface: "eth0"
          depends:
            - "server_b"
            - "server_c"

        - name: "server_b"
          mac: "11:22:33:44:55:66"
          interface: "eth0"
          depends:
            - "server_c"

        - name: "server_c"
          mac: "22:33:44:55:66:77"
          interface: "eth0"
        "#;

        // Parse the YAML string into the expected structure
        let servers: Vec<Server> =
            serde_yaml_ng::from_str(yaml_data).expect("Failed to parse YAML");

        // Expected topologically sorted order (server_c first, then server_b, then server_a)
        let expected_order = vec!["server_c", "server_b", "server_a"];

        // Call the function to get the wakeup order
        let result = determine_wakeup_order(&servers).expect("Failed to determine wakeup order");

        // Check that the result matches the expected order
        assert_eq!(
            result.into_iter().map(|s| s.name).collect::<Vec<String>>(),
            expected_order
        );
    }

    #[tokio::test]
    async fn test_http_health_check_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/health")
            .with_status(200)
            .with_body("healthy")
            .create_async()
            .await;

        let url = format!("{}/health", server.url());

        // Status and regex match
        let status = Some(200);
        let regex = Some(Regex::new("health").unwrap());

        let result = http_health_check(&url, status, regex).await;
        assert!(result);

        // Just status
        let status = Some(200);
        let regex = None;

        let result = http_health_check(&url, status, regex).await;
        assert!(result);

        // Just regex
        let status = None;
        let regex = Some(Regex::new("health").unwrap());

        let result = http_health_check(&url, status, regex).await;
        assert!(result);
    }

    #[tokio::test]
    async fn test_http_health_check_fail() {
        // Mock a failed response
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/health")
            .with_status(503)
            .with_body("Service Unavailable")
            .create_async()
            .await;

        let url = format!("{}/health", server.url());

        // Status and regex match
        let status = Some(200);
        let regex = Some(Regex::new("health").unwrap());

        let result = http_health_check(&url, status, regex).await;
        assert!(!result);

        // Just status
        let status = Some(200);
        let regex = None;

        let result = http_health_check(&url, status, regex).await;
        assert!(!result);

        // Just regex
        let status = None;
        let regex = Some(Regex::new("health").unwrap());

        let result = http_health_check(&url, status, regex).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_port_health_check_success() {
        // Set up a mock TCP listener on an available port
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();

        let port = listener.local_addr().unwrap().port();
        let ip = "127.0.0.1";

        // Simulate the health check
        let result = port_health_check(&ip, port).await;
        assert!(result);

        drop(listener); // Close the listener
    }

    #[tokio::test]
    async fn test_port_health_check_fail() {
        // Set up a mock TCP listener on an available port
        // Could probably just pick a random port, but I want to make sure
        // we don't accidentally pick a port that's in use by other process
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let ip = "127.0.0.1";

        let result = port_health_check(&ip, port).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_shell_health_check_success() {
        let command = "echo 'hello'";

        // Status and regex
        let status = Some(0);
        let regex = Some(Regex::new("hello").unwrap());
        let result = shell_health_check(&command, status, regex).await;

        assert!(result);

        // Just status
        let status = Some(0);
        let regex = None;
        let result = shell_health_check(&command, status, regex).await;

        assert!(result);

        // Just regex
        let status = None;
        let regex = Some(Regex::new("hello").unwrap());
        let result = shell_health_check(&command, status, regex).await;

        assert!(result);
    }

    #[tokio::test]
    async fn test_shell_health_check_fail() {
        let command = "echo 'hello'";

        // Regex does not match
        let status = None;
        let regex = Some(Regex::new("world").unwrap());
        let result = shell_health_check(&command, status, regex).await;
        assert!(!result);

        // Status does not match
        let status = Some(1);
        let regex = None;
        let result = shell_health_check(&command, status, regex).await;
        assert!(!result);

        // Regex and status does not match
        let status = Some(1);
        let regex = Some(Regex::new("world").unwrap());
        let result = shell_health_check(&command, status, regex).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_health_check_timeout() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/")
            .with_status(500)
            .expect(3)
            .create_async()
            .await;

        let yaml_data = r#"
        - name: "timeout_test_server"
          mac: "00:11:22:33:44:55"
          interface: "eth0"
          vlan: 100
          depends: []
          check:
            - type: http
              url: <url>
              status: 200
              retry: 800 ms
              timeout: 2s
    "#;

        let yaml_data = yaml_data.replace("<url>", &server.url());

        let servers: Vec<Server> =
            serde_yaml_ng::from_str(&yaml_data).expect("Failed to parse YAML");

        let server_state = Arc::new(RwLock::new(servers));

        let start_time = Instant::now();
        let result = perform_health_checks(server_state.clone(), 0).await;

        assert!(start_time.elapsed() >= std::time::Duration::from_secs(2));
        assert_eq!(result, ServerStatus::TimedOut);
        // Also check that the number of retries is correct
        mock.assert();
    }
}
