/// Metadata for a slash command.
pub struct CommandDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub category: &'static str,
    pub args_hint: Option<&'static str>,
}

/// Registry of all available slash commands.
/// Used for /help display and tab completion.
pub struct CommandRegistry {
    commands: Vec<CommandDef>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn register(&mut self, cmd: CommandDef) {
        self.commands.push(cmd);
    }

    /// Resolve a command name (including aliases) to its canonical name.
    pub fn resolve(&self, input: &str) -> Option<&'static str> {
        let input = input.to_lowercase();
        for cmd in &self.commands {
            if cmd.name == input {
                return Some(cmd.name);
            }
            for alias in cmd.aliases {
                if *alias == input {
                    return Some(cmd.name);
                }
            }
        }
        None
    }

    /// Get command metadata by canonical name.
    pub fn get(&self, name: &str) -> Option<&CommandDef> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// List all commands, grouped by category.
    pub fn list_by_category(&self) -> Vec<(&'static str, Vec<&CommandDef>)> {
        let mut categories: std::collections::BTreeMap<&str, Vec<&CommandDef>> =
            std::collections::BTreeMap::new();
        for cmd in &self.commands {
            categories.entry(cmd.category).or_default().push(cmd);
        }
        categories.into_iter().collect()
    }

    /// Extract all canonical command names and aliases prefixed with '/'
    pub fn all_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for cmd in &self.commands {
            names.push(format!("/{}", cmd.name));
            for alias in cmd.aliases {
                names.push(format!("/{}", alias));
            }
        }
        names
    }

    /// Extract all canonical command names/aliases along with their descriptions
    pub fn all_command_hints(&self) -> Vec<(String, String)> {
        let mut hints = Vec::new();
        for cmd in &self.commands {
            let desc = cmd.description.to_string();
            hints.push((format!("/{}", cmd.name), desc.clone()));
            for alias in cmd.aliases {
                hints.push((format!("/{}", alias), desc.clone()));
            }
        }
        hints
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        let mut registry = Self::new();

        // Session management
        registry.register(CommandDef {
            name: "new",
            aliases: &["reset"],
            description: "结束当前会话，创建新会话",
            category: "会话管理",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "clear",
            aliases: &[],
            description: "清屏并创建新会话",
            category: "会话管理",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "resume",
            aliases: &[],
            description: "切换到指定会话",
            category: "会话管理",
            args_hint: Some("<id|title>"),
        });
        registry.register(CommandDef {
            name: "sessions",
            aliases: &["history"],
            description: "列出最近的会话",
            category: "会话管理",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "session",
            aliases: &[],
            description: "显示当前会话详情",
            category: "会话管理",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "tree",
            aliases: &[],
            description: "显示会话树、事件时间线，或从指定事件分叉",
            category: "会话管理",
            args_hint: Some("[events|fork <event_index> [title]]"),
        });
        registry.register(CommandDef {
            name: "rename",
            aliases: &["title"],
            description: "重命名当前会话",
            category: "会话管理",
            args_hint: Some("<title>"),
        });
        registry.register(CommandDef {
            name: "branch",
            aliases: &["fork"],
            description: "从当前位置分叉新会话",
            category: "会话管理",
            args_hint: Some("[title]"),
        });
        registry.register(CommandDef {
            name: "compress",
            aliases: &["compact"],
            description: "手动触发上下文压缩",
            category: "会话管理",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "retry",
            aliases: &[],
            description: "丢弃上一轮，重新回答",
            category: "会话管理",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "undo",
            aliases: &[],
            description: "撤销上一轮（回到用户输入前）",
            category: "会话管理",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "save",
            aliases: &["export"],
            description: "导出当前会话为 JSON",
            category: "会话管理",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "snapshot",
            aliases: &["checkpoint"],
            description: "保存当前事件流检查点",
            category: "会话管理",
            args_hint: Some("[summary|list]"),
        });
        registry.register(CommandDef {
            name: "rollback",
            aliases: &["rewind"],
            description: "回滚到最近或指定检查点",
            category: "会话管理",
            args_hint: Some("[n|event_index|list]"),
        });
        registry.register(CommandDef {
            name: "report",
            aliases: &[],
            description: "生成当前案件 Markdown 报告",
            category: "会话管理",
            args_hint: None,
        });

        // Interaction control
        registry.register(CommandDef {
            name: "queue",
            aliases: &[],
            description: "排队下一轮 Watson 输入",
            category: "交互控制",
            args_hint: Some("[message]"),
        });
        registry.register(CommandDef {
            name: "steer",
            aliases: &[],
            description: "给 Holmes 下一轮推理注入修正/偏好",
            category: "交互控制",
            args_hint: Some("[note]"),
        });

        // Goal system
        registry.register(CommandDef {
            name: "goal",
            aliases: &[],
            description: "设定/查看/清除自主完成目标",
            category: "Goal",
            args_hint: Some("[condition|clear]"),
        });

        // Config & Model
        registry.register(CommandDef {
            name: "model",
            aliases: &[],
            description: "查看或切换模型",
            category: "配置",
            args_hint: Some("[name|list]"),
        });
        registry.register(CommandDef {
            name: "provider",
            aliases: &[],
            description: "显示当前 provider 信息",
            category: "配置",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "mode",
            aliases: &[],
            description: "切换工作模式 (pentest|audit|reverse|research)",
            category: "配置",
            args_hint: Some("<mode>"),
        });
        registry.register(CommandDef {
            name: "config",
            aliases: &[],
            description: "显示或修改当前配置",
            category: "配置",
            args_hint: Some("[set <key> <value>]"),
        });
        registry.register(CommandDef {
            name: "permissions",
            aliases: &["permission", "perm"],
            description: "查看或调整工具权限策略",
            category: "配置",
            args_hint: Some("[mode|allow|deny|remove|auto-read-only|reset]"),
        });
        registry.register(CommandDef {
            name: "guards",
            aliases: &["guard"],
            description: "查看或开关 GuardChain 防护层",
            category: "配置",
            args_hint: Some("[enable|disable|all|window]"),
        });

        // Tools
        registry.register(CommandDef {
            name: "tools",
            aliases: &[],
            description: "列出可用工具或查看工具详情",
            category: "工具",
            args_hint: Some("[name]"),
        });
        registry.register(CommandDef {
            name: "mcp",
            aliases: &[],
            description: "MCP 服务器管理",
            category: "工具",
            args_hint: Some("[reload]"),
        });

        // Info
        registry.register(CommandDef {
            name: "help",
            aliases: &[],
            description: "显示所有可用命令",
            category: "信息",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "status",
            aliases: &[],
            description: "当前会话状态（ID、模式、轮次、token）",
            category: "信息",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "dashboard",
            aliases: &[],
            description: "显示当前画报",
            category: "信息",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "usage",
            aliases: &[],
            description: "Token 使用量和费用统计",
            category: "信息",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "version",
            aliases: &[],
            description: "显示版本信息",
            category: "信息",
            args_hint: None,
        });

        // Workflow control
        registry.register(CommandDef {
            name: "workflows",
            aliases: &[],
            description: "列出可用工作流及其描述",
            category: "工作流",
            args_hint: None,
        });
        registry.register(CommandDef {
            name: "workflow",
            aliases: &[],
            description: "手动触发指定工作流",
            category: "工作流",
            args_hint: Some("<name>"),
        });
        registry.register(CommandDef {
            name: "chat",
            aliases: &[],
            description: "切换回对话模式（默认）",
            category: "工作流",
            args_hint: None,
        });

        // Exit
        registry.register(CommandDef {
            name: "quit",
            aliases: &["exit", "q"],
            description: "退出 Holmes",
            category: "退出",
            args_hint: None,
        });

        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_aliases() {
        let registry = CommandRegistry::default();
        assert_eq!(registry.resolve("quit"), Some("quit"));
        assert_eq!(registry.resolve("exit"), Some("quit"));
        assert_eq!(registry.resolve("q"), Some("quit"));
        assert_eq!(registry.resolve("fork"), Some("branch"));
        assert_eq!(registry.resolve("compact"), Some("compress"));
        assert_eq!(registry.resolve("history"), Some("sessions"));
        assert_eq!(registry.resolve("reset"), Some("new"));
        assert_eq!(registry.resolve("nonexistent"), None);
    }

    #[test]
    fn test_list_by_category() {
        let registry = CommandRegistry::default();
        let cats = registry.list_by_category();
        assert!(!cats.is_empty());
        let cat_names: Vec<&str> = cats.iter().map(|(n, _)| *n).collect();
        assert!(cat_names.contains(&"会话管理"));
        assert!(cat_names.contains(&"Goal"));
        assert!(cat_names.contains(&"配置"));
        assert!(cat_names.contains(&"工具"));
        assert!(cat_names.contains(&"信息"));
        assert!(cat_names.contains(&"工作流"));
    }

    #[test]
    fn test_all_commands_have_description() {
        let registry = CommandRegistry::default();
        for cmd in &registry.commands {
            assert!(
                !cmd.description.is_empty(),
                "command '{}' has no description",
                cmd.name
            );
            assert!(
                !cmd.category.is_empty(),
                "command '{}' has no category",
                cmd.name
            );
        }
    }
}
