use crate::connector::{Connector, WarehouseInfo};
use crate::yaml_parsers::agent_parser::MessagePair;
use crate::yaml_parsers::config_parser::ParsedConfig;
use crate::yaml_parsers::entity_parser::EntityConfig;
use arrow::record_batch::RecordBatch;
use arrow_cast::pretty::{pretty_format_batches, print_batches};
use minijinja::context;
use reqwest::Client;
use serde_json::json;
use std::env;
use std::error::Error;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};

pub struct Agent {
    client: Client,
    model_ref: String,
    model_key: String,
    instructions: MessagePair,
    tools: Vec<String>,
    postscript: MessagePair,
    entity_config: EntityConfig,
    connector: Connector,
    warehouse_info: Option<WarehouseInfo>,
}

impl Agent {
    pub fn new(parsed_config: ParsedConfig, entity_config: EntityConfig) -> Self {
        let model_ref = parsed_config.model.model_ref;
        let model_key_var = parsed_config.model.key_var;
        let model_key = env::var(&model_key_var).expect("Environment variable not found");
        let instructions = parsed_config.agent_config.instructions;

        let tools = parsed_config.agent_config.tools;
        let postscript = parsed_config.agent_config.postscript;

        Agent {
            client: Client::new(),
            connector: Connector::new(parsed_config.warehouse),
            warehouse_info: None,
            model_ref,
            model_key,
            instructions,
            tools,
            postscript,
            entity_config,
        }
    }

    pub async fn init(&mut self) -> Result<(), Box<dyn Error>> {
        self.warehouse_info = Some(self.connector.load_warehouse_info().await);
        Ok(())
    }

    pub async fn generate_ai_response(
        &self,
        system_message: &str,
        user_input: &str,
    ) -> Result<String, Box<dyn Error>> {
        let request = json!({
            "model": self.model_ref,
            "messages": [
                {
                    "role": "system",
                    "content": system_message
                },
                {
                    "role": "user",
                    "content": user_input
                }
            ]
        });

        let response = self
            .client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.model_key))
            .json(&request)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        Ok(response["choices"][0]["message"]["content"]
            .as_str()
            .expect("Failed to get content from OpenAI")
            .to_string())
    }

    pub async fn interpret_results(
        &self,
        input: &str,
        sql_query: &str,
        result_string: &str,
    ) -> Result<String, Box<dyn Error>> {
        let (system_message, user_message) = self
            .compile_postscript(input, Some(sql_query), Some(result_string), None)
            .await?;

        self.generate_ai_response(&system_message, &user_message)
            .await
    }

    pub async fn generate_sql_query(&mut self, input: &str) -> Result<String, Box<dyn Error>> {
        let (system_message, user_message) = self.compile_instructions(input).await?;
        log::debug!("Instructions: {}", system_message);
        log::debug!("User message: {}", user_message);
        self.generate_ai_response(&system_message, &user_message)
            .await
    }

    async fn execute_bigquery_query(
        &self,
        query: &str,
    ) -> Result<Vec<RecordBatch>, Box<dyn Error>> {
        let result = self.connector.run_query(query).await?;
        println!("\n\x1b[1;32mResults:\x1b[0m");
        print_batches(&result)?;
        Ok(result)
    }

    pub async fn execute_chain(
        &mut self,
        input: &str,
        query: Option<String>,
    ) -> Result<(), Box<dyn Error>> {
        // Uses `instructions` from agent config
        let sql_query = if let Some(provided_query) = query {
            provided_query
        } else {
            self.generate_sql_query(input).await?
        };

        println!("\n\x1b[1;32mSQL query:\x1b[0m");
        self.print_colored_sql(&sql_query);

        // Execute query
        let result_string: String = match self.execute_bigquery_query(&sql_query).await {
            Ok(record_batches) => pretty_format_batches(&record_batches)?.to_string(),
            Err(error) => format!("Error executing query: {}", error),
        };

        // Uses `postscript` from agent config
        let interpretation = self
            .interpret_results(input, &sql_query, &result_string)
            .await?;

        // Print interpretation with green bold formatting and a newline above
        println!("\n\x1b[1;32mInterpretation:\x1b[0m");
        println!("{}", interpretation);

        Ok(())
    }

    fn print_colored_sql(&self, sql: &str) {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let syntax = ps.find_syntax_by_extension("sql").unwrap();
        let mut h = HighlightLines::new(syntax, &ts.themes["base16-ocean.dark"]);

        for line in LinesWithEndings::from(sql) {
            let ranges: Vec<(Style, &str)> = h.highlight_line(line, &ps).unwrap();
            let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
            print!("{}", escaped);
        }
        println!();
    }

    pub async fn compile_instructions(
        &mut self,
        input: &str,
    ) -> Result<(String, String), Box<dyn Error>> {
        if self.warehouse_info.is_none() {
            self.init().await?;
        }
        let ctx = context! {
            input => input,
            entities => self.entity_config.format_entities(),
            metrics => self.entity_config.format_metrics(),
            analyses => self.entity_config.format_analyses(),
            schema => self.entity_config.format_schema(),
            warehouse => self.warehouse_info
        };

        self.instructions.compile(ctx)
    }

    pub async fn compile_postscript(
        &self,
        input: &str,
        sql_query: Option<&str>,
        sql_results: Option<&str>,
        retrieve_results: Option<&str>,
    ) -> Result<(String, String), Box<dyn Error>> {
        let ctx = context! {
            input => input,
            sql_query => sql_query,
            sql_results => sql_results,
            retrieve_results => retrieve_results,
        };

        self.postscript.compile(ctx)
    }
}
