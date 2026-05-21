//! Tool registry — tool pool registration + manifest-driven activation


//!


//! Two-step process:


//! 1. `all_builtin_tools()` builds the complete tool pool


//! 2. `activate()` filters by manifest declarations and applies security decorators





use rollball_core::tools::traits::Tool;


use rollball_core::AgentManifest;


use std::sync::Arc;





use crate::tools::wrappers::{PathGuardedTool, RateLimitedTool};


use crate::tools::workspace_resolver::{WorkspaceDir, WorkspaceAccess, WorkspaceResolver};





/// Tool registry


pub struct ToolRegistry {


    tools: Vec<Arc<dyn Tool>>,


}





impl ToolRegistry {


    /// Create new empty registry


    pub fn new() -> Self {


        Self { tools: Vec::new() }


    }





    /// Register a tool


    pub fn register(&mut self, tool: Arc<dyn Tool>) {


        self.tools.push(tool);


    }





    /// Get tool by name


    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {


        self.tools.iter().find(|t| t.name() == name).cloned()


    }





    /// Get all registered tools


    pub fn all(&self) -> &[Arc<dyn Tool>] {


        &self.tools


    }





    /// Activate tools based on manifest declarations.


    ///


    /// Tool activation IS authorization — no separate permission check needed.


    ///


    /// Steps:


    /// 1. If manifest.tools is non-empty, filter to only declared tools


    /// 2. Load workspace directories from `.agent_workspaces.json`


    /// 3. Apply security decorators: PathGuarded → RateLimited


    pub fn activate(


        &self,


        manifest: &AgentManifest,


        work_dir: &str,


        max_calls_per_minute: u32,


    ) -> (Vec<Arc<dyn Tool>>, WorkspaceResolver) {


        let filtered: Vec<Arc<dyn Tool>> = if manifest.tools.is_empty() {


            // No tool declarations → all tools available


            self.tools.clone()


        } else {


            // Filter to only declared tools.


            // Shell tools may be declared as "shell" in the manifest but


            // registered as "bash"/"powershell" by the platform detector.


            self.tools


                .iter()


                .filter(|tool| {


                    manifest.has_tool(&tool.name())


                        || is_shell_alias(&tool.name(), manifest)


                })


                .cloned()


                .collect()


        };





        // Build workspace resolver (single source of truth for directory resolution)


        let resolver = WorkspaceResolver::new(work_dir);


        let allowed_dirs = resolver.allowed_dirs().to_vec();





        // Apply security decorators


        let tools = filtered


            .into_iter()


            .map(|tool| {


                // Layer 1: Path guard (for filesystem tools)


                let path_guarded = Arc::new(PathGuardedTool::new(tool, allowed_dirs.clone())) as Arc<dyn Tool>;





                // Layer 2: Rate limit


                Arc::new(RateLimitedTool::new(path_guarded, max_calls_per_minute)) as Arc<dyn Tool>


            })


            .collect();





        (tools, resolver)


    }





    /// List all registered tool names


    pub fn tool_names(&self) -> Vec<String> {


        self.tools.iter().map(|t| t.name()).collect()


    }


}





impl Default for ToolRegistry {


    fn default() -> Self {


        Self::new()


    }


}





#[allow(clippy::items_after_test_module)]


#[cfg(test)]


mod tests {


    use super::*;


    use async_trait::async_trait;


    use rollball_core::tools::traits::{ToolResult, ToolSpec};


    use serde_json::Value;





    struct MockTool {


        name: String,


    }





    #[async_trait]


    impl Tool for MockTool {


        fn spec(&self) -> ToolSpec {


            ToolSpec {


                name: self.name.clone(),


                description: format!("Mock tool {}", self.name),


                input_schema: serde_json::json!({"type": "object"}),


            }


        }


        async fn execute(&self, _params: Value) -> rollball_core::error::Result<ToolResult> {


            Ok(ToolResult {


                ok: true,


                content: format!("Mock {} executed", self.name),


                error: None,


                token_usage: None,


            })


        }


    }





    fn create_registry() -> ToolRegistry {


        let mut reg = ToolRegistry::new();


        reg.register(Arc::new(MockTool {


            name: "shell".to_string(),


        }));


        reg.register(Arc::new(MockTool {


            name: "calculator".to_string(),


        }));


        reg.register(Arc::new(MockTool {


            name: "weather".to_string(),


        }));


        reg.register(Arc::new(MockTool {


            name: "memory_store".to_string(),


        }));


        reg


    }





    fn manifest_with_tools(tool_names: &[&str]) -> AgentManifest {


        let tools_toml = tool_names


            .iter()


            .map(|name| format!("[[tools]]\nname = \"{}\"", name))


            .collect::<Vec<_>>()


            .join("\n");





        let toml_str = format!(


            r#"


            agent_id = "com.test.agent"


            version = "1.0.0"


            name = "Test Agent"


            description = "Test"


            author = "test"


            runtime_version = "0.1.0"





            [llm]


            provider = "openai"


            model = "gpt-4"





            [[permissions]]


            type = "Shell"





            [[permissions]]


            type = "Network"





            [[permissions]]


            type = "MemoryWrite"





            {}


            "#,


            tools_toml


        );


        AgentManifest::from_toml(&toml_str).unwrap()


    }





    #[test]


    fn test_registry_register_and_get() {


        let reg = create_registry();


        assert!(reg.get("shell").is_some());


        assert!(reg.get("calculator").is_some());


        assert!(reg.get("nonexistent").is_none());


    }





    #[test]


    fn test_registry_tool_names() {


        let reg = create_registry();


        let names = reg.tool_names();


        assert!(names.contains(&"shell".to_string()));


        assert!(names.contains(&"calculator".to_string()));


    }





    #[test]


    fn test_registry_activate_with_manifest_tools() {


        let reg = create_registry();


        let manifest = manifest_with_tools(&["shell", "calculator"]);


        let (activated, _) = reg.activate(&manifest, "/tmp/work", 60);


        assert_eq!(activated.len(), 2);


        let names: Vec<String> = activated.iter().map(|t| t.name()).collect();


        assert!(names.contains(&"shell".to_string()));


        assert!(names.contains(&"calculator".to_string()));


        assert!(!names.contains(&"weather".to_string()));


    }





    #[test]


    fn test_registry_activate_no_manifest_tools() {


        let reg = create_registry();


        let toml_str = r#"


            agent_id = "com.test.agent"


            version = "1.0.0"


            name = "Test Agent"


            description = "Test"


            author = "test"


            runtime_version = "0.1.0"





            [llm]


            provider = "openai"


            model = "gpt-4"


        "#;


        let manifest = AgentManifest::from_toml(toml_str).unwrap();


        let (activated, _) = reg.activate(&manifest, "/tmp/work", 60);


        assert_eq!(activated.len(), 4); // All tools available


    }





    #[test]


    fn test_registry_default() {


        let reg = ToolRegistry::default();


        assert!(reg.all().is_empty());


    }


}





/// Check whether a tool is a shell variant that should match a "shell"


/// declaration in the manifest.


///


/// Platform-aware shell tools (bash, powershell) fill the same role as the


/// legacy unified "shell" tool.  If the manifest declares "shell" but the


/// platform detector registered "bash" + "powershell", they should still pass


/// the activation filter.


fn is_shell_alias(tool_name: &str, manifest: &AgentManifest) -> bool {


    matches!(tool_name, "bash" | "powershell" | "pwsh" | "shell")


        && (manifest.has_tool("shell") || manifest.has_tool("pwsh"))


}

