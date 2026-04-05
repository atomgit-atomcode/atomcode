//! Python dev server detection.
//!
//! Covers Django, Flask, uvicorn, gunicorn, and Python's built-in HTTP server.
//! No pre-command needed: Python is interpreted.

use super::DetectedServer;

pub fn detect(cmd: &str) -> Option<DetectedServer> {
    let trimmed = cmd.trim();

    // Skip package manager commands — "pip install uvicorn" is not a server start
    if trimmed.contains("pip ") || trimmed.contains("pip3 ")
        || trimmed.contains("conda ") || trimmed.contains("poetry ")
    {
        return None;
    }

    if trimmed.contains("uvicorn ") {
        return Some(DetectedServer {
            label: "Uvicorn",
            default_port: 8000,
            pre_command: None,
        });
    }

    if trimmed.contains("gunicorn ") {
        return Some(DetectedServer {
            label: "Gunicorn",
            default_port: 8000,
            pre_command: None,
        });
    }

    if trimmed.contains("flask run") {
        return Some(DetectedServer {
            label: "Flask",
            default_port: 5000,
            pre_command: None,
        });
    }

    if trimmed.contains("manage.py runserver") {
        return Some(DetectedServer {
            label: "Django",
            default_port: 8000,
            pre_command: None,
        });
    }

    if trimmed.contains("python -m http") || trimmed.contains("python3 -m http") {
        return Some(DetectedServer {
            label: "Python HTTP Server",
            default_port: 8000,
            pre_command: None,
        });
    }

    None
}
