use chrono::prelude::{DateTime, Utc};
use std::path::PathBuf;

use crate::{
    ai::{self, agent::LLMAgent},
    config::model::{FileFormat, ProjectPath},
    db::{
        conversations::{create_conversation, get_conversation_by_agent},
        message::save_message,
    },
};
use async_stream::stream;
use axum::response::IntoResponse;
use axum::{extract, Json};
use axum_streams::StreamBodyAs;
use sea_orm::prelude::DateTimeWithTimeZone;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;
#[derive(Serialize)]
pub struct ConversationItem {
    title: String,
    id: Uuid,
}

#[derive(Serialize)]
pub struct ListConversationResponse {
    conversations: Vec<ConversationItem>,
}

#[derive(Deserialize)]
pub struct ListConversationRequest {}

#[derive(Serialize)]
pub struct AskResponse {
    pub answer: String,
}

#[derive(Deserialize)]
pub struct AskRequest {
    pub question: String,
    pub agent: String,
    pub title: String,
}

async fn get_agent(agent_path: &str) -> Box<dyn LLMAgent + Send> {
    let file_path = ProjectPath::get_path(agent_path);

    (ai::setup_agent(Some(&file_path), &FileFormat::Markdown)
        .await
        .unwrap()) as _
}

pub async fn ask(extract::Json(payload): extract::Json<AskRequest>) -> impl IntoResponse {
    let conversation = get_conversation_by_agent(payload.agent.as_str()).await;
    let conversation_id: Uuid;
    match conversation {
        Some(c) => {
            conversation_id = c.id;
        }
        None => {
            let new_conversation = create_conversation(&payload.agent, &payload.title).await;
            conversation_id = new_conversation.id;
        }
    }
    let question = save_message(conversation_id, &payload.question, true).await;
    let s = stream! {
        yield Message {
            content: payload.question.clone(),
            id: question.id,
            is_human: question.is_human,
            created_at: question.created_at,
        };

    let agent = get_agent(&payload.agent).await;
    let result: String =agent.request(&payload.question).await.unwrap().output;
    let answer = save_message(
        conversation_id,
        &result,
        false,
    ).await;

    let chunks = vec![answer.content];
    let msgs = chunks.into_iter().map(|chunk| {

        Message {
            content: chunk.to_string(),
            id: answer.id,
            is_human: answer.is_human,
            created_at: answer.created_at,
        }
    }).collect::<Vec<_>>();
    for msg in msgs {
        tokio::time::sleep(Duration::from_millis(10)).await;
        yield msg;
    }
    };
    return StreamBodyAs::json_nl(s);
}

#[derive(Serialize)]
struct Message {
    content: String,
    id: Uuid,
    is_human: bool,
    created_at: DateTimeWithTimeZone,
}

#[derive(Serialize)]
pub struct AgentItem {
    updated_at: DateTime<Utc>,
    path: String,
}

#[derive(Serialize)]
pub struct ListAgentResponse {
    agents: Vec<AgentItem>,
}

pub async fn list() -> Json<ListAgentResponse> {
    let project_path = ProjectPath::get();

    let agent_files = find_agent_files(&project_path);
    let mut agents = Vec::new();

    for path in agent_files {
        if let Ok(metadata) = path.metadata() {
            if let Ok(modified) = metadata.modified() {
                let relative_path = path
                    .strip_prefix(&project_path)
                    .unwrap_or(&path)
                    .to_path_buf();
                agents.push(AgentItem {
                    path: relative_path.to_string_lossy().to_string(),
                    updated_at: modified.into(),
                });
            }
        }
    }

    agents.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Json(ListAgentResponse { agents })
}

fn find_agent_files(dir: &PathBuf) -> Vec<PathBuf> {
    let mut agent_files = Vec::new();

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                agent_files.extend(find_agent_files(&path));
            } else if path.is_file()
                && path.extension().and_then(|s| s.to_str()) == Some("yml")
                && path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.ends_with(".agent.yml"))
                    .unwrap_or(false)
            {
                agent_files.push(path);
            }
        }
    }

    agent_files
}
