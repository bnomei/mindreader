use crate::config::GLOBAL_LAYER;
use std::fmt;

#[derive(Debug)]
pub struct LayerError(pub String);

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LayerError {}

pub fn bound_project(project: &str) -> &str {
    project
}

pub fn visible_layers(project: &str) -> Vec<String> {
    vec![GLOBAL_LAYER.to_string(), project.to_string()]
}

pub fn default_write_layer(project: &str) -> String {
    project.to_string()
}

pub fn assert_writable_layer(layer: &str, project: &str) -> Result<(), LayerError> {
    if layer == GLOBAL_LAYER || layer == project {
        return Ok(());
    }
    Err(LayerError(format!(
        "layer not allowed: {layer}. Writes must use \"{GLOBAL_LAYER}\" or the bound project \"{project}\"."
    )))
}
