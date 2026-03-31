//! JavaScript/TypeScript dev server detection.
//!
//! Covers npm/yarn/pnpm dev servers, Vite, Next.js, and Node.js.
//! No pre-command needed: Vite/webpack have built-in HMR, no compile step.

use super::DetectedServer;

pub fn detect(cmd: &str) -> Option<DetectedServer> {
    let trimmed = cmd.trim();

    // Vite (via npm/yarn/pnpm or direct npx)
    if trimmed.contains("vite") && !trimmed.contains("build") {
        return Some(DetectedServer {
            label: "Vite",
            default_port: 5173,
            pre_command: None,
        });
    }

    // Next.js
    if trimmed.contains("next dev") || trimmed.contains("next start") {
        return Some(DetectedServer {
            label: "Next.js",
            default_port: 3000,
            pre_command: None,
        });
    }

    // npm/yarn/pnpm dev commands
    if trimmed.contains("npm run dev") || trimmed.contains("yarn dev") || trimmed.contains("pnpm dev") {
        return Some(DetectedServer {
            label: "Node.js Dev Server",
            default_port: 3000,
            pre_command: None,
        });
    }

    if trimmed.contains("npm start") {
        return Some(DetectedServer {
            label: "Node.js",
            default_port: 3000,
            pre_command: None,
        });
    }

    // npx (generic — could be anything)
    if trimmed.contains("npx ") {
        return Some(DetectedServer {
            label: "npx",
            default_port: 3000,
            pre_command: None,
        });
    }

    // node server.js / node app.js / node index.js
    if trimmed.contains("node ") && (trimmed.contains("server") || trimmed.contains("app") || trimmed.contains("index")) {
        return Some(DetectedServer {
            label: "Node.js",
            default_port: 3000,
            pre_command: None,
        });
    }

    None
}
