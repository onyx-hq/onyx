use crate::{
    ai::utils::{record_batches_to_json, record_batches_to_markdown, record_batches_to_table},
    config::model::{FileFormat, OutputFormat},
    connector::load_result,
};
use std::collections::HashMap;

use super::{anonymizer::base::Anonymizer, toolbox::ToolBox, tools::Tool};
use crate::theme::*;
use async_openai::{
    config::{OpenAIConfig, OPENAI_API_BASE},
    types::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
        ChatCompletionRequestUserMessageArgs, ChatCompletionResponseMessage, ChatCompletionTool,
        ChatCompletionToolArgs, ChatCompletionToolType, CreateChatCompletionRequestArgs,
        FunctionObjectArgs, ResponseFormat, ResponseFormatJsonSchema,
    },
    Client,
};
use async_trait::async_trait;
use log::debug;
use pyo3::pyclass;
use schemars::{schema_for, JsonSchema};
use serde::Deserialize;
use serde_json::json;

const MAX_DISPLAY_ROWS: usize = 100;

#[pyclass(module = "onyx_py")]
pub struct AgentResult {
    #[pyo3(get)]
    pub output: String,
    #[pyo3(get)]
    pub steps: Vec<Step>,
}

#[pyclass(module = "onyx_py")]
#[derive(Clone)]
pub struct Step {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    output: String,
}

#[derive(Deserialize, Debug, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilePathOutput {
    pub file_path: String,
}

#[async_trait]
pub trait LLMAgent {
    async fn request(&self, input: &str) -> anyhow::Result<AgentResult>;
}

pub struct OpenAIAgent<T> {
    tools: ToolBox<T>,
    client: Client<OpenAIConfig>,
    model: String,
    system_instruction: String,
    max_tries: u8,
    // @TODO: Lets clean this up once we finalize the output format
    output_format: OutputFormat,
    anonymizer: Option<Box<dyn Anonymizer + Send + Sync>>,
    file_format: FileFormat,
}

impl<T> OpenAIAgent<T> {
    pub fn new(
        model: String,
        api_url: Option<String>,
        api_key: String,
        tools: ToolBox<T>,
        system_instruction: String,
        output_format: OutputFormat,
        anonymizer: Option<Box<dyn Anonymizer + Send + Sync>>,
        file_format: FileFormat,
    ) -> Self {
        let client_config = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(api_url.unwrap_or(OPENAI_API_BASE.to_string()));
        let client = Client::with_config(client_config);
        let max_tries = 5;

        OpenAIAgent {
            client,
            tools,
            model,
            max_tries,
            system_instruction,
            output_format,
            anonymizer,
            file_format,
        }
    }

    async fn completion_request(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        tools: Vec<ChatCompletionTool>,
        response_format: Option<ResponseFormat>,
    ) -> anyhow::Result<ChatCompletionResponseMessage> {
        let mut request_builder = CreateChatCompletionRequestArgs::default();
        if tools.is_empty() {
            request_builder.model(self.model.clone()).messages(messages);
        } else {
            request_builder
                .model(self.model.clone())
                .tools(tools)
                .parallel_tool_calls(false)
                .messages(messages);
        }
        if response_format.is_some() {
            request_builder.response_format(response_format.unwrap());
        }

        let request = request_builder.build().unwrap();

        let response = self
            .client
            .chat() // Get the API "group" (completions, images, etc.) from the client
            .create(request) // Make the API call in that "group"
            .await?;

        Ok(response.choices[0].message.clone())
    }

    fn spec_serializer(
        name: String,
        description: String,
        parameters: serde_json::Value,
    ) -> ChatCompletionTool {
        ChatCompletionToolArgs::default()
            .r#type(ChatCompletionToolType::Function)
            .function(
                FunctionObjectArgs::default()
                    .name(name)
                    .description(description)
                    .parameters(parameters)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }
}

#[async_trait]
impl<T> LLMAgent for OpenAIAgent<T>
where
    T: Tool + Send + Sync,
{
    async fn request(&self, input: &str) -> anyhow::Result<AgentResult> {
        let system_message = self.system_instruction.to_string();
        debug!("System message: {}", system_message);
        let anonymized_items = HashMap::new();
        let (anonymized_system_message, anonymized_items) = match self.anonymizer {
            Some(ref anonymizer) => anonymizer.anonymize(&system_message, Some(anonymized_items)),
            None => Ok((system_message.clone(), anonymized_items)),
        }?;
        let (anonymized_user_message, anonymized_items) = match self.anonymizer {
            Some(ref anonymizer) => anonymizer.anonymize(input, Some(anonymized_items)),
            None => Ok((input.to_string(), anonymized_items)),
        }?;

        let messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessageArgs::default()
                .name("onyx")
                .content(anonymized_system_message)
                .build()?
                .into(),
            ChatCompletionRequestUserMessageArgs::default()
                .name("Human")
                .content(anonymized_user_message)
                .build()?
                .into(),
        ];
        let tools = self.tools.to_spec(OpenAIAgent::<T>::spec_serializer);

        let mut tries: u8 = 0;
        let mut output = "Something went wrong".to_string();
        let mut tool_returns = Vec::<ChatCompletionRequestMessage>::new();
        let mut tool_calls = Vec::<ChatCompletionRequestMessage>::new();
        let mut steps: Vec<Step> = vec![];

        let mut contextualize_anonymized_items = anonymized_items.clone();
        while tries < self.max_tries {
            let message_with_replies =
                [messages.clone(), tool_calls.clone(), tool_returns.clone()].concat();
            tool_returns.clear();
            tool_calls.clear();
            log::debug!("Start completion request {:?}", message_with_replies);
            let response_format: Option<ResponseFormat> = match self.output_format {
                OutputFormat::Default => None,
                OutputFormat::File => {
                    let schema = json!(schema_for!(FilePathOutput));
                    log::info!("Schema: {}", schema);
                    Some(ResponseFormat::JsonSchema {
                        json_schema: ResponseFormatJsonSchema {
                            name: "file_path".to_string(),
                            description: Some(
                                "Path to the arrow file containing the query results".to_string(),
                            ),
                            schema: Some(schema),
                            strict: Some(true),
                        },
                    })
                }
            };
            let ret_message = self
                .completion_request(message_with_replies, tools.clone(), response_format)
                .await?;

            output = ret_message
                .content
                .unwrap_or("Empty response from OpenAI".to_string())
                .clone();
            let tool_call_requests = ret_message.tool_calls.unwrap_or_default();
            log::info!(
                "Number of tool calls: {} on {}",
                &tool_call_requests.len(),
                tries
            );
            for tool in tool_call_requests.clone() {
                let mut tool_ret: String = self
                    .tools
                    .run_tool(tool.function.name.clone(), tool.function.arguments.clone())
                    .await;
                if self.anonymizer.is_some() {
                    let result = self
                        .anonymizer
                        .as_ref()
                        .unwrap()
                        .anonymize(&tool_ret, Some(contextualize_anonymized_items.clone()))?;
                    contextualize_anonymized_items.extend(result.1);
                    tool_ret = result.0;
                }

                steps.push(Step {
                    name: tool.function.name.clone(),
                    output: tool_ret.clone(),
                });

                tool_returns.push(
                    ChatCompletionRequestToolMessageArgs::default()
                        .tool_call_id(tool.id.clone())
                        .content(tool_ret)
                        .build()?
                        .into(),
                );
            }

            if tool_returns.is_empty() {
                break;
            }
            tool_calls.push(
                ChatCompletionRequestAssistantMessageArgs::default()
                    .tool_calls(tool_call_requests.clone())
                    .build()?
                    .into(),
            );

            tries += 1;
        }
        let mut parsed_output = map_output(&output, &self.output_format, &self.file_format).await?;
        parsed_output = match self.anonymizer {
            Some(ref anonymizer) => {
                anonymizer.deanonymize(&parsed_output, &contextualize_anonymized_items)
            }
            None => parsed_output,
        };
        println!("{}", "\nOutput:".primary());
        println!("{}", &parsed_output);
        return Ok(AgentResult {
            output: parsed_output,
            steps,
        });
    }
}

async fn map_output(
    output: &str,
    output_format: &OutputFormat,
    file_format: &FileFormat,
) -> anyhow::Result<String> {
    match output_format {
        OutputFormat::Default => Ok(output.to_string()),
        OutputFormat::File => {
            log::info!("File path: {}", output);
            let file_output = serde_json::from_str::<FilePathOutput>(output)?;
            let (batches, schema) = load_result(&file_output.file_path)?;
            let mut dataset = batches;
            let mut truncated = false;
            if !dataset.is_empty() && dataset[0].num_rows() > MAX_DISPLAY_ROWS {
                dataset = vec![dataset[0].slice(0, MAX_DISPLAY_ROWS)];
                truncated = true;
            }

            let batches_display = record_batches_to_table(&dataset, &schema)?;
            let markdown_table = record_batches_to_markdown(&dataset, &schema)?;
            let json_blob = record_batches_to_json(&dataset)?;

            println!(
                "\n{}",
                format_table_output(&batches_display.to_string(), truncated).text()
            );

            match file_format {
                FileFormat::Json => Ok(format_table_output(&json_blob, truncated)),
                FileFormat::Markdown => {
                    Ok(format_table_output(&markdown_table.to_string(), truncated))
                }
            }
        }
    }
}

fn format_table_output(table: &str, truncated: bool) -> String {
    if truncated {
        format!(
            "{}\n{}",
            format!(
                "Results have been truncated. Showing only the first {} rows.",
                MAX_DISPLAY_ROWS
            ),
            table
        )
    } else {
        table.to_string()
    }
}
