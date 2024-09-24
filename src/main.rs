mod agent;
mod connector;
mod init;
mod search;
mod yaml_parsers;

use clap::Parser;
use connector::Connector;
use skim::prelude::*;
use std::error::Error;
use std::path::PathBuf;
use std::fs;
use std::ffi::OsStr;

use crate::agent::Agent;
use crate::search::search_files;
use crate::yaml_parsers::config_parser::{get_config_path, parse_config, Config};
use crate::yaml_parsers::entity_parser::parse_entity_config_from_scope;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// The question to ask or command to execute
    #[clap(default_value = "")]
    input: String,

    /// Output format: 'text' (default) or 'code' for SQL
    #[clap(long, value_name = "FORMAT")]
    output: Option<String>,

    /// Subcommand
    #[clap(subcommand)]
    command: Option<SubCommand>,

    /// Specify a custom agent configuration
    #[clap(long, value_name = "AGENT_NAME")]
    agent: Option<String>,
}

#[derive(Parser, Debug)]
enum SubCommand {
    Init,
    ListDatasets,
    ListTables,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    match args.command {
        Some(SubCommand::Init) => match init::init() {
            Ok(_) => println!("Initialization complete"),
            Err(e) => eprintln!("Initialization failed: {}", e),
        },
        Some(SubCommand::ListTables) => {
            let config_path = get_config_path();
            let config = parse_config(config_path)?;
            let parsed_config = config.load_config(None)?;
            let ddls = Connector::new(parsed_config.warehouse).get_schemas().await;
            print!("{:?}", ddls);
        },
        Some(SubCommand::ListDatasets) => {
            let config_path = get_config_path();
            let config = parse_config(config_path)?;
            let parsed_config = config.load_config(None)?;
            let datasets = Connector::new(parsed_config.warehouse).list_datasets().await;
            print!("{:?}", datasets);
        },
        None => {
            let config_path = get_config_path();

            // Parse the config.yaml file into strings
            let config = parse_config(config_path)?;

            let parsed_config = config.load_config(args.agent.as_deref().filter(|s| !s.is_empty()))?;

            // Parse the entity config from the scope defined in agent config
            let entity_config = parse_entity_config_from_scope(
                &parsed_config.agent_config.scope,
                &config.defaults.project_path,
            )?;

            // Create the agent from the parsed config and entity config
            let mut agent = Agent::new(parsed_config, entity_config);

            if !args.input.is_empty() {
                agent.execute_chain(&args.input, None).await?;
            } else {
                let project_path = PathBuf::from(&config.defaults.project_path);
                match search_files(&project_path)? {
                    Some(content) => {
                        agent.execute_chain("", Some(content)).await?;
                    }
                    None => println!(""),
                }
            }
        }
    }

    Ok(())
}
