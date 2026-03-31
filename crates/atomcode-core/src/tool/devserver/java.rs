//! Java/JVM dev server detection.
//!
//! Covers Maven (spring-boot:run), Gradle (bootRun), and direct java -jar.
//! Pre-command: compile before starting to catch errors early.

use super::DetectedServer;

pub fn detect(cmd: &str) -> Option<DetectedServer> {
    let trimmed = cmd.trim();

    if trimmed.contains("spring-boot:run") {
        let pre = if trimmed.contains("mvn") || trimmed.contains("mvnw") {
            Some("mvn compile -q")
        } else if trimmed.contains("gradle") || trimmed.contains("gradlew") {
            Some("gradle compileJava -q")
        } else {
            Some("mvn compile -q")
        };
        return Some(DetectedServer {
            label: "Spring Boot",
            default_port: 8080,
            pre_command: pre,
        });
    }

    if trimmed.contains("gradle bootRun") || trimmed.contains("gradlew bootRun") {
        return Some(DetectedServer {
            label: "Spring Boot (Gradle)",
            default_port: 8080,
            pre_command: Some("gradle compileJava -q"),
        });
    }

    if trimmed.contains("java -jar") || trimmed.contains("java -cp") {
        return Some(DetectedServer {
            label: "Java Application",
            default_port: 8080,
            pre_command: None, // jar is already compiled
        });
    }

    None
}
