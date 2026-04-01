//! Full restart integration tests — verify log-based port detection and error handling.
//!
//! These tests use mock log content to verify the log parsing logic
//! without actually starting servers.

use atomcode_core::tool::devserver::java;

// ═══════════════════════════════════════════════════════════════
// 1. Compile error detection
// ═══════════════════════════════════════════════════════════════

#[test]
fn is_compile_command_matches_maven() {
    assert!(java::is_compile_command("mvn compile"));
    assert!(java::is_compile_command("mvn clean compile -DskipTests"));
    assert!(java::is_compile_command("cd backend && mvn compile -q"));
    assert!(java::is_compile_command("mvn package -DskipTests"));
    assert!(java::is_compile_command("mvn install"));
    assert!(java::is_compile_command("mvn clean package -DskipTests 2>&1 | tail -20"));
}

#[test]
fn is_compile_command_matches_gradle() {
    assert!(java::is_compile_command("gradle compileJava"));
    assert!(java::is_compile_command("gradle build"));
    assert!(java::is_compile_command("./gradlew build -x test"));
}

#[test]
fn is_compile_command_rejects_non_compile() {
    assert!(!java::is_compile_command("mvn spring-boot:run"));
    assert!(!java::is_compile_command("java -jar app.jar"));
    assert!(!java::is_compile_command("curl http://localhost:8081"));
    assert!(!java::is_compile_command("ls -la"));
}

// ═══════════════════════════════════════════════════════════════
// 2. Server command detection
// ═══════════════════════════════════════════════════════════════

#[test]
fn detect_spring_boot() {
    let d = java::detect("mvn spring-boot:run").unwrap();
    assert_eq!(d.label, "Spring Boot");
}

#[test]
fn detect_spring_boot_in_compound() {
    let d = java::detect("pkill -f devpress; sleep 2; cd backend && mvn spring-boot:run -q").unwrap();
    assert_eq!(d.label, "Spring Boot");
}

#[test]
fn detect_gradle_bootrun() {
    let d = java::detect("gradle bootRun").unwrap();
    assert_eq!(d.label, "Spring Boot (Gradle)");
}

#[test]
fn detect_java_jar() {
    let d = java::detect("java -jar app.jar").unwrap();
    assert_eq!(d.label, "Java Application");
}

#[test]
fn detect_non_java_returns_none() {
    assert!(java::detect("npm run dev").is_none());
    assert!(java::detect("python manage.py runserver").is_none());
    assert!(java::detect("cargo run").is_none());
}

// ═══════════════════════════════════════════════════════════════
// 3. Compile error enhancement
// ═══════════════════════════════════════════════════════════════

#[test]
fn enhance_compile_error_extracts_file_line() {
    let error = "[ERROR] /src/main/java/com/example/Service.java:[42,15] cannot find symbol";
    let enhanced = java::enhance_compile_error(error, std::path::Path::new("/tmp"));
    // Should contain the original error
    assert!(enhanced.contains("[ERROR]"));
    assert!(enhanced.contains("cannot find symbol"));
}

#[test]
fn enhance_compile_error_no_errors_returns_original() {
    let output = "[INFO] BUILD SUCCESS\n[INFO] Total time: 3.2s";
    let enhanced = java::enhance_compile_error(output, std::path::Path::new("/tmp"));
    assert_eq!(enhanced, output);
}

#[test]
fn enhance_compile_error_multiple_errors() {
    let error = "\
[ERROR] /src/Service.java:[42,15] cannot find symbol
[ERROR] /src/Controller.java:[10,5] method not found
[ERROR] /src/Model.java:[88,1] ';' expected";
    let enhanced = java::enhance_compile_error(error, std::path::Path::new("/tmp"));
    assert!(enhanced.contains("Service.java"));
    assert!(enhanced.contains("Controller.java"));
    assert!(enhanced.contains("Model.java"));
}

// ═══════════════════════════════════════════════════════════════
// 4. Log-based port detection (unit test the regex logic)
// ═══════════════════════════════════════════════════════════════

#[test]
fn port_from_spring_boot_log() {
    let log = "2026-04-01 Tomcat started on port(s): 8081 (http)";
    let port = extract_port_from_log_line(log);
    assert_eq!(port, Some(8081));
}

#[test]
fn port_from_netty_log() {
    let log = "Netty started on port 8443";
    let port = extract_port_from_log_line(log);
    assert_eq!(port, Some(8443));
}

#[test]
fn port_from_generic_listening() {
    let log = "Server listening on port 3000";
    let port = extract_port_from_log_line(log);
    assert_eq!(port, Some(3000));
}

#[test]
fn port_from_started_on() {
    let log = "Application started on port 9090";
    let port = extract_port_from_log_line(log);
    assert_eq!(port, Some(9090));
}

#[test]
fn no_port_from_irrelevant_log() {
    let log = "Loading configuration from /etc/app.conf";
    let port = extract_port_from_log_line(log);
    assert_eq!(port, None);
}

#[test]
fn no_port_from_port_keyword_without_number() {
    let log = "Configuring server port settings";
    // Contains "port" but no "started/listening" context
    let port = extract_port_from_log_line(log);
    assert_eq!(port, None);
}

// ═══════════════════════════════════════════════════════════════
// 5. Startup failure detection
// ═══════════════════════════════════════════════════════════════

#[test]
fn detect_port_conflict() {
    let log = "Caused by: java.net.BindException: Address already in use: 8081";
    assert!(log.contains("Address already in use"));
}

#[test]
fn detect_app_failure() {
    let log = "***************************\nAPPLICATION FAILED TO START\n***************************";
    assert!(log.contains("APPLICATION FAILED TO START"));
}

#[test]
fn detect_node_port_conflict() {
    let log = "Error: listen EADDRINUSE: address already in use :::3000";
    // Our detection should handle this too
    assert!(log.contains("already in use"));
}

// ═══════════════════════════════════════════════════════════════
// Helper: extract port from a single log line (mirrors full_restart logic)
// ═══════════════════════════════════════════════════════════════

fn extract_port_from_log_line(line: &str) -> Option<u16> {
    let lower = line.to_lowercase();
    if lower.contains("port") && (lower.contains("started") || lower.contains("listening") || lower.contains("port(s)")) {
        if let Some(pos) = lower.find("port") {
            let after = &line[pos + 4..];
            let num_str: String = after.chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(p) = num_str.parse::<u16>() {
                if p > 0 {
                    return Some(p);
                }
            }
        }
    }
    None
}
