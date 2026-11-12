//! MCP board server (stdio), built on the official `rmcp` SDK (v5 issue:
//! the hand-rolled JSON-RPC spoke protocol 2024-11-05 and drifted from the
//! spec; now the SDK negotiates a current version and generates schemas).
//!
//! Agents get the board as MCP tools by spawning this binary. The role
//! identity: the owner mints a per-role token at launch
//! (AI_COMMANDER_BOARD_TOKEN env) and the server forwards it over IPC;
//! in direct mode (no owner) the role comes from AI_COMMANDER_ROLE.
//!
//! Tools (snake_case — MCP clients accept [a-zA-Z0-9_-] only):
//!   board_create_task, board_update_task, board_comment, board_handoff,
//!   board_artifact, board_read_inbox, board_ask_human

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::Value;

use ai_commander::kanban::Kanban;
use ai_commander::kanban::store::BoardStore;

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(
    description = "Create a kanban task (Backlog). depends_on: task ids that must finish first; parent_id: delegate from your task (depth is capped)."
)]
struct CreateTaskArgs {
    #[schemars(description = "Short task title")]
    title: String,
    #[serde(default)]
    #[schemars(description = "Task ids that must be Done/Cancelled first")]
    depends_on: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Parent task id for delegation")]
    parent_id: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(
    description = "Advance a task. You may only start/finish a task assigned to your role, and never approve your own work."
)]
struct UpdateTaskArgs {
    task_id: String,
    #[schemars(description = "promote | start | finish | approve | rework | cancel")]
    action: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(description = "Post a comment to the task thread (visible to humans and other agents).")]
struct CommentArgs {
    task_id: String,
    text: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(
    description = "Hand the task to another role. summary (>=10 chars) and open_questions form the handoff contract; the task returns to Ready for the new owner."
)]
struct HandoffArgs {
    task_id: String,
    to_role: String,
    summary: String,
    #[serde(default)]
    open_questions: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(
    description = "Attach a concrete output of your work to the task: a file path, branch name, link or short text. Artifacts survive handoffs and are shown to the next owner."
)]
struct ArtifactArgs {
    task_id: String,
    #[serde(default)]
    #[schemars(description = "file | branch | link | text (default text)")]
    kind: String,
    value: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(description = "Read task thread entries newer than since_ms (epoch ms, default 0).")]
struct ReadInboxArgs {
    task_id: String,
    #[serde(default)]
    since_ms: i64,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(description = "Escalate: mark the task NeedsHuman with your question in text.")]
struct AskHumanArgs {
    task_id: String,
    text: String,
}

struct BoardService {
    /// direct mode: an open redb store + the role from the env
    direct: Option<parking_lot::Mutex<BoardStore>>,
    /// IPC mode: the owner's role token
    role_token: String,
    /// role for direct mode
    role: String,
}

impl BoardService {
    /// Route one tool call to the owner's IPC (client mode) or the local
    /// store (direct mode). Soft errors become tool errors the agent sees.
    fn dispatch(&self, name: &str, args: Value) -> Result<String, ErrorData> {
        let out = match self.direct.as_ref().map(parking_lot::Mutex::lock) {
            // v8 #6: delegate to the ONE board_tool_call — it has the
            // Actor::Human check for NeedsHuman promote that the local
            // copy was missing (agents could self-unpark in direct mode)
            Some(mut board) => ai_commander::kanban::board_tool_call(
                name,
                &args,
                &ai_commander::kanban::Actor::Role(self.role.clone()),
                &mut board,
            ),
            // IPC client: forward to the owner's store, acting as the role
            // the token authorizes (v4: the body `role` field is never used)
            None => {
                if self.role_token.is_empty() {
                    Err("board owner is serving IPC but AI_COMMANDER_BOARD_TOKEN is not set".into())
                } else {
                    match ai_commander::kanban::ipc::call_owner(name, &args, &self.role_token) {
                        Some(Ok(text)) => Ok(text),
                        Some(Err(e)) => Err(e),
                        None => Err("board owner is not reachable (TUI/daemon stopped?)".into()),
                    }
                }
            }
        };
        out.map_err(|e| ErrorData::internal_error(e, None))
    }
}

#[tool_router]
impl BoardService {
    #[tool(name = "board_create_task")]
    async fn create_task(
        &self,
        Parameters(CreateTaskArgs {
            title,
            depends_on,
            parent_id,
        }): Parameters<CreateTaskArgs>,
    ) -> Result<String, ErrorData> {
        let mut args = serde_json::Map::new();
        args.insert("title".into(), Value::String(title));
        if !depends_on.is_empty() {
            args.insert(
                "depends_on".into(),
                Value::Array(depends_on.into_iter().map(Value::String).collect()),
            );
        }
        if !parent_id.is_empty() {
            args.insert("parent_id".into(), Value::String(parent_id));
        }
        self.dispatch("board_create_task", Value::Object(args))
    }

    #[tool(name = "board_update_task")]
    async fn update_task(
        &self,
        Parameters(UpdateTaskArgs { task_id, action }): Parameters<UpdateTaskArgs>,
    ) -> Result<String, ErrorData> {
        self.dispatch(
            "board_update_task",
            serde_json::json!({"task_id": task_id, "action": action}),
        )
    }

    #[tool(name = "board_comment")]
    async fn comment(
        &self,
        Parameters(CommentArgs { task_id, text }): Parameters<CommentArgs>,
    ) -> Result<String, ErrorData> {
        self.dispatch(
            "board_comment",
            serde_json::json!({"task_id": task_id, "text": text}),
        )
    }

    #[tool(name = "board_handoff")]
    async fn handoff(
        &self,
        Parameters(HandoffArgs {
            task_id,
            to_role,
            summary,
            open_questions,
        }): Parameters<HandoffArgs>,
    ) -> Result<String, ErrorData> {
        self.dispatch(
            "board_handoff",
            serde_json::json!({
                "task_id": task_id, "to_role": to_role,
                "summary": summary, "open_questions": open_questions
            }),
        )
    }

    #[tool(name = "board_artifact")]
    async fn artifact(
        &self,
        Parameters(ArtifactArgs {
            task_id,
            kind,
            value,
        }): Parameters<ArtifactArgs>,
    ) -> Result<String, ErrorData> {
        self.dispatch(
            "board_artifact",
            serde_json::json!({"task_id": task_id, "kind": kind, "value": value}),
        )
    }

    #[tool(name = "board_read_inbox")]
    async fn read_inbox(
        &self,
        Parameters(ReadInboxArgs { task_id, since_ms }): Parameters<ReadInboxArgs>,
    ) -> Result<String, ErrorData> {
        self.dispatch(
            "board_read_inbox",
            serde_json::json!({"task_id": task_id, "since_ms": since_ms}),
        )
    }

    #[tool(name = "board_ask_human")]
    async fn ask_human(
        &self,
        Parameters(AskHumanArgs { task_id, text }): Parameters<AskHumanArgs>,
    ) -> Result<String, ErrorData> {
        self.dispatch(
            "board_ask_human",
            serde_json::json!({"task_id": task_id, "text": text}),
        )
    }
}

#[tool_handler]
impl rmcp::ServerHandler for BoardService {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        use rmcp::model::{Implementation, ServerCapabilities};
        let server_info = Implementation::new("ai-commander-board", env!("CARGO_PKG_VERSION"));
        rmcp::model::InitializeResult::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_instructions(
            "The kanban board of this repository. Tools mutate tasks;              your role authorizes what you may start/finish/approve.",
        )
        .with_server_info(server_info)
    }
}

#[tokio::main]
async fn main() {
    // v4: if a board owner (TUI or daemon) is serving IPC, work as its
    // client — never open the redb store (exclusive lock) and never create
    // a second store. Direct mode only when nobody owns the board.
    let direct = if ai_commander::kanban::ipc::owner_alive() {
        let role_token = std::env::var("AI_COMMANDER_BOARD_TOKEN").unwrap_or_default();
        if role_token.is_empty() {
            eprintln!(
                "board-mcp: an owner is serving IPC but AI_COMMANDER_BOARD_TOKEN is not set;"
            );
            eprintln!("       the owner must launch this server with a minted role token.");
            std::process::exit(1);
        }
        None
    } else {
        match BoardStore::open_redb(&Kanban::path()) {
            Ok(b) => Some(parking_lot::Mutex::new(b)),
            Err(e) => {
                // redb takes an exclusive lock (review v2 Б1): the TUI or
                // daemon owns the DB but serves no IPC (older owner build).
                eprintln!("board-mcp: cannot open the kanban store: {e:#}");
                eprintln!("hint: the board is likely open in ai-commander (TUI or daemon).");
                eprintln!(
                    "       stop the owner process or set AI_COMMANDER_KANBAN_DB to a separate copy."
                );
                std::process::exit(1);
            }
        }
    };
    let role_token = std::env::var("AI_COMMANDER_BOARD_TOKEN").unwrap_or_default();
    let role = std::env::var("AI_COMMANDER_ROLE").unwrap_or_default();

    let service = BoardService {
        direct,
        role_token,
        role,
    };
    let running = service
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
        .expect("serve MCP over stdio");
    running.waiting().await.expect("MCP session ended");
}
