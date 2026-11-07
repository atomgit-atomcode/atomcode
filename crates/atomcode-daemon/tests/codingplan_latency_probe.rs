//! Temporary probe: time the stored-auth CodingPlan calls the usage page uses.

use std::time::Instant;

#[test]
fn measure_stored_auth_latency() {
    let product = atomcode_coding::config::product_dirs_from_env();
    let client = match atomcode_codingplan::Client::from_stored_auth(product.user()) {
        Ok(client) => client,
        Err(error) => {
            println!("stored auth unavailable: {error}");
            return;
        }
    };
    for name in ["status_v2", "usage"] {
        let start = Instant::now();
        let outcome = match name {
            "status_v2" => client.status_v2().map(|_| "ok".to_string()),
            _ => client.usage().map(|_| "ok".to_string()),
        };
        let elapsed = start.elapsed();
        match outcome {
            Ok(state) => println!("{name}: {elapsed:?} -> {state}"),
            Err(error) => println!("{name}: {elapsed:?} -> error: {error}"),
        }
    }
}