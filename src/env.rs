use serde::Deserialize;

fn default_insecure() -> bool {
    false
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct Env {
    aws_server_address: String,
    aws_access_key: String,
    aws_secret_key: String,
    #[serde(default="default_insecure")]
    aws_insecure: bool,
}
impl Env {
    pub fn new() -> Result<Self, envy::Error> {
        envy::prefixed("CAFCE_").from_env::<Env>()
    }
}
