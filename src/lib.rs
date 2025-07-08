pub mod config;
pub mod health;
pub mod admin;

pub use config::*;
pub use health::*;

use serde_json::{Map, Value};
pub fn json_to_template_data(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        _ => {
            let mut result = Map::new();
            result.insert("data".to_string(), value.clone());
            result
        }
    }
}


