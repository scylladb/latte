use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug, Default, Serialize, Deserialize)]
pub struct DbConnectionConf {}
