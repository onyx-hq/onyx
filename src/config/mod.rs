use std::path::PathBuf;
pub mod model;
mod parser;
pub mod validate;
use garde::Validate;

use anyhow;
use model::{AgentConfig, Config, Model, Retrieval, Warehouse, Workflow};

use dirs::home_dir;
use parser::{parse_agent_config, parse_workflow_config};
use serde::Deserialize;
use std::{fs, io};
use validate::ValidationContext;

// These are settings stored as strings derived from the config.yml file's defaults section
#[derive(Debug, Deserialize)]
pub struct Defaults {
    pub agent: String,
    pub project_path: PathBuf,
}

impl Defaults {
    pub fn expand_project_path(&mut self) {
        if let Some(str_path) = self.project_path.to_str() {
            if str_path.starts_with("~") {
                if let Some(home) = home_dir() {
                    self.project_path = home.join(str_path.trim_start_matches("~"));
                }
            }
        }
    }
}

pub fn get_config_path() -> PathBuf {
    std::env::current_dir()
        .expect("Could not get current directory")
        .join("config.yml")
}

#[derive(Debug)]
pub struct ParsedConfig {
    pub agent_config: AgentConfig,
    pub model: Model,
    pub warehouse: Warehouse,
    pub retrieval: Retrieval,
}

impl Config {
    pub fn validate_workflow(&self, workflow: &Workflow) -> anyhow::Result<()> {
        let context = ValidationContext {
            config: self.clone(),
        };
        match workflow.validate_with(&context) {
            Ok(_) => Ok(()),
            Err(e) => Err(anyhow::anyhow!(
                "Invalid workflow: {} \n{}",
                workflow.name,
                e
            )),
        }
    }

    pub fn validate_workflows(&self) -> anyhow::Result<()> {
        for workflow_file in self.list_workflows()? {
            let workflow = self.load_workflow(&workflow_file)?;
            self.validate_workflow(&workflow)?;
        }
        Ok(())
    }

    pub fn get_agents_dir(&self) -> PathBuf {
        PathBuf::from(&self.project_path).join("agents")
    }

    pub fn get_sql_dir(&self) -> PathBuf {
        PathBuf::from(&self.project_path).join("data")
    }

    pub fn load_config(
        &self,
        agent_file: Option<&PathBuf>,
    ) -> anyhow::Result<(AgentConfig, String)> {
        let agent_file = if let Some(file) = agent_file {
            file
        } else {
            &self
                .get_agents_dir()
                .join(format!("{}.agent.yml", self.defaults.agent))
        };

        if !agent_file.exists() {
            return Err(anyhow::Error::msg(format!(
                "Agent configuration file not found: {:?}",
                agent_file
            )));
        }

        let agent_config = parse_agent_config(&agent_file.to_string_lossy())?;

        let agent_name = agent_file.file_stem().unwrap().to_str().unwrap();
        let agent_name = agent_name.strip_suffix(".agent").unwrap_or(agent_name);

        Ok((agent_config, agent_name.to_owned()))
    }

    pub fn list_workflows(&self) -> anyhow::Result<Vec<PathBuf>> {
        let workflow_dir = PathBuf::from(&self.project_path).join("workflows");

        let mut workflows = vec![];
        for entry in fs::read_dir(workflow_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                if let Some(file_name) = path.file_name() {
                    if let Some(file_name_str) = file_name.to_str() {
                        if file_name_str.ends_with(".workflow.yml") {
                            workflows.push(path);
                        }
                    }
                }
            }
        }

        Ok(workflows)
    }

    pub fn load_workflow(&self, workflow_path: &PathBuf) -> anyhow::Result<Workflow> {
        if !workflow_path.exists() {
            return Err(anyhow::Error::msg(format!(
                "Workflow configuration file not found: {:?}",
                workflow_path
            )));
        }

        let workflow_name = workflow_path.file_stem().unwrap().to_str().unwrap();
        let workflow_name = workflow_name
            .strip_suffix(".workflow")
            .unwrap_or(workflow_name);

        let workflow_config =
            parse_workflow_config(workflow_name, &workflow_path.to_string_lossy())?;

        Ok(workflow_config)
    }

    pub fn find_model(&self, model_name: &str) -> anyhow::Result<Model> {
        self.models
            .iter()
            .find(|m| {
                match match m {
                    Model::OpenAI { name, .. } => name,
                    Model::Ollama { name, .. } => name,
                } {
                    name => name == model_name,
                }
            })
            .cloned()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Default model not found").into()
            })
    }

    pub fn find_warehouse(&self, warehouse_name: &str) -> anyhow::Result<Warehouse> {
        self.warehouses
            .iter()
            .find(|w| w.name == warehouse_name)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Default warehouse not found").into()
            })
    }

    pub fn find_retrieval(&self, retrieval_name: &str) -> anyhow::Result<Retrieval> {
        self.retrievals
            .iter()
            .find(|m| m.name == retrieval_name)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Default retrieval not found").into()
            })
    }
}

pub fn load_config() -> anyhow::Result<Config> {
    let config_path: PathBuf = get_config_path();
    let config = parse_config(&config_path)?;

    Ok(config)
}

pub fn parse_config(config_path: &PathBuf) -> anyhow::Result<Config> {
    let config_str = fs::read_to_string(config_path)?;
    let result = serde_yaml::from_str::<Config>(&config_str);
    match result {
        Ok(mut config) => {
            config.project_path = std::env::current_dir().expect("Could not get current directory");
            let context = ValidationContext {
                config: config.clone(),
            };
            let validation_result = config.validate_with(&context);
            drop(context);
            match validation_result {
                Ok(_) => Ok(config),
                Err(e) => Err(anyhow::anyhow!("Invalid configuration: \n{}", e)),
            }
        }
        Err(e) => {
            let mut raw_error = e.to_string();
            raw_error = raw_error.replace("usize", "unsigned integer");
            Err(anyhow::anyhow!(
                "Failed to parse config file:\n{}",
                raw_error
            ))
        }
    }
}
