//! Harness-owned registry assembled from downstream host tool providers.
//!
//! The host remains the source of truth for tool availability. The harness
//! builds a normalized registry view so orchestration can reason about:
//!
//! - which functions are executable locally
//! - which functions may continue in the background after the inline budget
//!   is exhausted

use std::collections::HashMap;
use std::sync::Arc;

use gemini_live::types::Tool;

use crate::error::HarnessError;
use crate::{NoopToolSource, ToolCapability, ToolDescriptor, ToolProvider};

/// One normalized function entry inside the harness registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredTool {
    pub function_name: String,
    pub capability: ToolCapability,
}

/// Normalized view built from one host-fed provider.
pub struct HarnessToolRegistry<P = NoopToolSource> {
    host_tools: Arc<P>,
    registrations: HashMap<String, RegisteredTool>,
}

impl<P> Clone for HarnessToolRegistry<P> {
    fn clone(&self) -> Self {
        Self {
            host_tools: Arc::clone(&self.host_tools),
            registrations: self.registrations.clone(),
        }
    }
}

impl Default for HarnessToolRegistry<NoopToolSource> {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessToolRegistry<NoopToolSource> {
    pub fn new() -> Self {
        Self {
            host_tools: Arc::new(NoopToolSource),
            registrations: HashMap::new(),
        }
    }
}

impl<P> HarnessToolRegistry<P>
where
    P: ToolProvider,
{
    pub fn with_host_tools(host_tools: Arc<P>) -> Result<Self, HarnessError> {
        let mut registrations = HashMap::new();

        for spec in host_tools.specifications() {
            let function_name = spec.function_name.clone();
            if registrations.contains_key(&function_name) {
                return Err(HarnessError::DuplicateToolFunction {
                    name: function_name,
                });
            }
            registrations.insert(
                function_name.clone(),
                RegisteredTool {
                    function_name,
                    capability: spec.capability,
                },
            );
        }

        Ok(Self {
            host_tools,
            registrations,
        })
    }

    pub fn host_tools(&self) -> &Arc<P> {
        &self.host_tools
    }

    pub fn advertised_tools(&self) -> Option<Vec<Tool>> {
        self.host_tools.advertised_tools()
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.host_tools.descriptors()
    }

    pub fn route(&self, function_name: &str) -> Option<&RegisteredTool> {
        self.registrations.get(function_name)
    }
}
