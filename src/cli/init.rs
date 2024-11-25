use crate::cli::get_config_path;
use crate::cli::model::{Config, Retrieval};
use crate::theme::*;
use include_dir::{include_dir, Dir};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::{fmt, fs};

use super::model::{Defaults, Model, Warehouse};

// Custom error type for better error handling
#[derive(Debug)]
pub enum InitError {
    IoError(io::Error),
    ExtractionError(String),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitError::IoError(err) => write!(f, "IO error: {}", err),
            InitError::ExtractionError(err) => write!(f, "Extraction error: {}", err),
        }
    }
}

impl From<io::Error> for InitError {
    fn from(error: io::Error) -> Self {
        InitError::IoError(error)
    }
}

static AGENTS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/agents");
static DATA_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/data");
static WORKFLOWS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/workflows");

// Helper function to prompt for input with default value
// Helper function to prompt for input with default value
fn prompt_with_default(prompt: &str, default: &str) -> io::Result<String> {
    print!("{} (default: {}): ", prompt, default);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_string();
    Ok(if input.is_empty() {
        default.to_string()
    } else {
        input
    })
}

// Function to collect warehouse configurations
fn collect_warehouses() -> Result<Vec<Warehouse>, InitError> {
    let mut warehouses = Vec::new();

    loop {
        println!("\nWarehouse {}:", warehouses.len() + 1);
        let warehouse = Warehouse {
            name: prompt_with_default("Name", "warehouse-1")?,
            r#type: prompt_with_default("Type", "bigquery")?,
            key_path: PathBuf::from(prompt_with_default("Key path", "bigquery.key")?),
            dataset: prompt_with_default("Dataset", "dbt_prod_core")?,
        };

        warehouses.push(warehouse);

        if !prompt_continue("Add another warehouse")? {
            break;
        }
    }

    Ok(warehouses)
}

// Function to collect model configurations
fn collect_models() -> Result<Vec<Model>, InitError> {
    let mut models = Vec::new();

    loop {
        println!("\nModel {}:", models.len() + 1);
        println!("Select model type:");
        println!("1. OpenAI");
        println!("2. Ollama");

        let model_type = prompt_with_default("Type (1 or 2)", "1")?;

        let model = match model_type.as_str() {
            "1" => Model::OpenAI {
                name: prompt_with_default("Name", "openai-4")?,
                model_ref: prompt_with_default("Model reference", "gpt-4")?,
                key_var: prompt_with_default("Key variable", "OPENAI_API_KEY")?,
            },
            "2" => Model::Ollama {
                name: prompt_with_default("Name", "llama3.2")?,
                model_ref: prompt_with_default("Model reference", "llama3.2:latest")?,
                api_key: prompt_with_default("API Key", "secret")?,
                api_url: prompt_with_default("API URL", "http://localhost:11434/v1")?,
            },
            _ => {
                println!("Invalid model type selected. Using OpenAI as default.");
                Model::OpenAI {
                    name: prompt_with_default("Name", "openai-4")?,
                    model_ref: prompt_with_default("Model reference", "gpt-4")?,
                    key_var: prompt_with_default("Key variable", "OPENAI_API_KEY")?,
                }
            }
        };

        models.push(model);

        if !prompt_continue("Add another model")? {
            break;
        }
    }

    Ok(models)
}

// Helper function to prompt for continuation
fn prompt_continue(message: &str) -> io::Result<bool> {
    print!("{} (y/n): ", message);
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_lowercase() == "y")
}
// Function to create and populate a directory
fn create_and_populate_directory(name: &str, dir: &Dir) -> Result<(), InitError> {
    fs::create_dir(name)?;
    dir.extract(name)
        .map_err(|e| InitError::ExtractionError(e.to_string()))?;
    println!(
        "{}",
        format!("Successfully extracted {} directory", name).success()
    );
    Ok(())
}

// Function to create directory structure
fn create_project_structure() -> Result<(), InitError> {
    let directories = [
        ("agents", &AGENTS_DIR),
        ("data", &DATA_DIR),
        ("workflows", &WORKFLOWS_DIR),
    ];

    for (name, dir) in directories.iter() {
        create_and_populate_directory(name, dir)?;
    }

    Ok(())
}

// Main initialization function
pub fn init() -> Result<(), InitError> {
    let config_path = get_config_path();

    if config_path.exists() {
        println!(
            "{}",
            format!(
                "config.yml found in {}. Only initializing current directory.",
                config_path.display().to_string().secondary()
            )
            .text()
        );
    } else {
        create_config_file(&config_path)?;
    }

    println!("{}", "Creating project scaffolding...".text());
    create_project_structure()?;
    println!("{}", "Project scaffolding created successfully".success());

    Ok(())
}

// Function to create configuration file
fn create_config_file(config_path: &Path) -> Result<(), InitError> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    println!("Please enter information for your warehouses:");
    let warehouses = collect_warehouses()?;

    println!("\nPlease enter information for your models:");
    let models = collect_models()?;

    let config = Config {
        warehouses,
        models,
        retrievals: vec![Retrieval {
            name: "default".to_string(),
            embed_model: "bge-small-en-v1.5".to_string(),
            rerank_model: "jina-reranker-v2-base-multiligual".to_string(),
            top_k: 10,
            factor: 5,
        }],
        defaults: Defaults {
            agent: "default".to_string(),
        },
        project_path: PathBuf::new(),
    };

    let yaml =
        serde_yaml::to_string(&config).map_err(|e| InitError::ExtractionError(e.to_string()))?;
    fs::write(config_path, yaml)?;

    println!(
        "{}",
        format!(
            "Created config.yml in {}",
            config_path.display().to_string().secondary()
        )
        .text()
    );

    Ok(())
}
