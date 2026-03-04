pub mod bash;
pub mod file;

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

use crate::provider::ToolDef;

/// A tool that the agent can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDef;

    /// Execute the tool. Returns (output, success).
    async fn execute(&self, input: &Value, cwd: &str) -> (String, bool);

    /// If the tool requires user permission for this particular input,
    /// return a description of the action. None = auto-approve.
    fn requires_permission(&self, _input: &Value) -> Option<String> {
        None
    }
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// Return tool definitions for the LLM API.
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// Build the default tool registry with all built-in tools.
    pub fn default_registry() -> Self {
        let mut reg = Self::new();
        reg.register(Box::new(bash::BashTool));
        reg.register(Box::new(file::ReadFileTool));
        reg.register(Box::new(file::WriteFileTool));
        reg.register(Box::new(file::EditFileTool));
        reg
    }

    /// Build a read-only registry (bash + read_file only) for planner and judge phases.
    pub fn read_only_registry() -> Self {
        let mut reg = Self::new();
        reg.register(Box::new(bash::BashTool));
        reg.register(Box::new(file::ReadFileTool));
        reg
    }
}
