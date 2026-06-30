use holmes_core::config::HolmesConfig;
use holmes_core::types::SessionMode;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_FILE_CHARS: usize = 12_000;
const MAX_TOTAL_CONTEXT_CHARS: usize = 36_000;
const MAX_SKILLS: usize = 32;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 320;

const NATIVE_CAPABILITIES: &str = r#"[Holmes native capabilities - always on]
These are not optional slash commands. Treat them as part of your default operating system:
- Maintain the case state automatically: goals, hypotheses, evidence, observations, failures, and memories.
- Use available tools and MCP tools proactively when they can reduce uncertainty or validate a claim.
- Ask Watson only when human judgment, authorization scope, or risk tradeoffs are genuinely blocking progress.
- Set or update goals when the user gives an objective, and evaluate completion without waiting for a manual command.
- Reflect and pivot when repeated attempts fail, evidence contradicts an assumption, or the current path stalls.
- Convert tool results into evidence, attack-surface updates, findings, and report-ready notes.
- Prefer investigation-native behavior over rigid workflows: infer the next best action from the case context.
- Keep security boundaries explicit. Do not perform actions outside the authorized scope."#;

/// Pentest 模式默认方法论（内化自 skills/pentest-lyan）。
/// 仅在 SessionMode::Pentest 下注入；其他 mode 不污染上下文。
/// 完整细节由模型按需 Read skills/pentest-lyan/SKILL.md 与 references/ 获取。
const PENTEST_METHODOLOGY: &str = r#"[Pentest methodology - default, no /command needed]
You operate web pentests with the Pentest-Lyan methodology baked in. Do not wait for the user to invoke it.

## 三阶段串行（目标驱动,非步骤）
1. **Discovery** — 完整阅读所有前端 JS,理解业务逻辑、签名机制、客户端可控参数;全量发现接口、会话池、权限矩阵;为每个 feature 基于 12 维度做威胁建模。读懂代码比测接口重要。
2. **Attack** — 按 feature 流水线遍历每个功能的 threat_model 逐项验证;深度验证业务影响,参数空间扩展,状态机遍历。
3. **Audit** — schema 校验、完整性审计、双格式报告(markdown 全面 + docx 交付)。

## 12 维度威胁建模(每个 feature 都过一遍)
data flow / permission boundary / resource ownership / state change / client-controlled values / concurrency / output display / auth & session / SSRF / injection surface / file ops / business logic

## Banned Patterns(绝不违反)
- 不得在未验证业务影响前将漏洞标记 confirmed —— 接口返回 200 ≠ 漏洞确认,必须验证数据库层值真的变了、对方数据真的被读到、状态真的被绕过
- not_vulnerable 记录必须附 unruled_out(还没排除的攻击面)
- coverage_note 必须回答三问:输入面 / 行为面 / 深度面;禁止"已覆盖""部分覆盖""基本完成"等模糊词
- 越权测试不得硬编码对象 ID,必须从 victim 的真实数据列表获取
- 配置类观察(CORS / 安全头 / 版本泄露)不计入 confirmed_vulns,不分配 VULN 编号,归入附录
- 不跟随到非目标资产(外部域 / CDN 重定向 → 记 blindspots BLOCKED)
- 不删除真实业务数据或其他用户预存数据
- 报告中 summary.json 所有 confirmed_vulns 必须从 validation_results 继承,不得编造
- 不未授权执行高危操作(批量删除 / 支付退款 / 修改权限)

## 反遗漏靠"三问 + 未排除面"
每个 feature 收口写 coverage_note,从输入面 / 行为面 / 深度面**泛化维度**自问,不点名具体漏洞。

完整方法论与 schema 详见 skills/pentest-lyan/SKILL.md;深入细节按阶段 Read references/{discovery-guide,attack-guide,threat-modeling,validation-guide,cross-role-testing,audit-guide,report-template}.md。"#;

#[derive(Debug, Clone, PartialEq, Eq)]
struct KnowledgeFile {
    label: String,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillSummary {
    name: String,
    description: String,
    path: String,
}

/// Build the prompt Holmes should see at session start.
///
/// This makes project instructions, local rules, and skill indexes part of
/// Holmes' native perception instead of requiring Watson to manually invoke
/// them every session. When `mode == SessionMode::Pentest`, the bundled
/// Pentest-Lyan methodology is also injected as a default operating standard.
pub fn build_system_prompt(
    base_prompt: &str,
    config: &HolmesConfig,
    cwd: &Path,
    mode: SessionMode,
) -> String {
    let mut sections = vec![
        base_prompt.trim().to_string(),
        NATIVE_CAPABILITIES.to_string(),
    ];

    if matches!(mode, SessionMode::Pentest) {
        sections.push(PENTEST_METHODOLOGY.to_string());
    }

    let knowledge = discover_knowledge_files(cwd);
    if !knowledge.is_empty() {
        sections.push(format_knowledge_files(
            "Auto-loaded Holmes knowledge",
            &knowledge,
        ));
    }

    if config.skills.auto_inject {
        let skills = discover_skills(cwd, &config.skills.dir);
        if !skills.is_empty() {
            sections.push(format_skill_index(&skills));
        }
    }

    enforce_total_limit(sections.join("\n\n"))
}

fn discover_knowledge_files(cwd: &Path) -> Vec<KnowledgeFile> {
    let mut files = Vec::new();

    push_file(&mut files, cwd, "HOLMES.md");
    push_file(&mut files, cwd, ".holmes/HOLMES.md");

    let rules_dir = cwd.join(".holmes").join("rules");
    for path in sorted_markdown_files(&rules_dir) {
        push_absolute_file(&mut files, cwd, &path, Some("rule"));
    }

    if !files
        .iter()
        .any(|file| file.label == "HOLMES.md" || file.label == ".holmes/HOLMES.md")
    {
        push_file(&mut files, cwd, "docs/HOLMES.md");
    }

    files
}

fn discover_skills(cwd: &Path, configured_dir: &str) -> Vec<SkillSummary> {
    let mut roots = Vec::new();
    let configured = PathBuf::from(configured_dir);
    roots.push(if configured.is_absolute() {
        configured
    } else {
        cwd.join(configured)
    });
    roots.push(cwd.join(".holmes").join("skills"));

    let mut skills = Vec::new();
    for root in roots {
        for path in skill_files(&root) {
            if skills.len() >= MAX_SKILLS {
                return skills;
            }
            if let Some(skill) = summarize_skill(cwd, &path) {
                if !skills
                    .iter()
                    .any(|existing: &SkillSummary| existing.path == skill.path)
                {
                    skills.push(skill);
                }
            }
        }
    }
    skills
}

fn skill_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }

    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_markdown(&path) {
                out.push(path);
            } else if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.is_file() {
                    out.push(skill_md);
                }
            }
        }
    }

    out.sort();
    out
}

fn summarize_skill(cwd: &Path, path: &Path) -> Option<SkillSummary> {
    let raw = fs::read_to_string(path).ok()?;
    let name = frontmatter_value(&raw, "name")
        .or_else(|| first_heading(&raw))
        .unwrap_or_else(|| {
            path.parent()
                .and_then(Path::file_name)
                .or_else(|| path.file_stem())
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "unnamed-skill".into())
        });
    let description = frontmatter_value(&raw, "description")
        .or_else(|| first_nonempty_body_line(&raw))
        .unwrap_or_else(|| "No description provided.".into());

    Some(SkillSummary {
        name: trim_inline(&name, 80),
        description: trim_inline(&description, MAX_SKILL_DESCRIPTION_CHARS),
        path: relative_label(cwd, path),
    })
}

fn frontmatter_value(raw: &str, key: &str) -> Option<String> {
    let mut lines = raw.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim() == key {
            return Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

fn first_heading(raw: &str) -> Option<String> {
    raw.lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn first_nonempty_body_line(raw: &str) -> Option<String> {
    let mut in_frontmatter = raw.trim_start().starts_with("---");
    for line in raw.lines() {
        let line = line.trim();
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        return Some(line.to_string());
    }
    None
}

fn sorted_markdown_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return files;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_markdown(&path) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn push_file(files: &mut Vec<KnowledgeFile>, cwd: &Path, relative: &str) {
    let path = cwd.join(relative);
    push_absolute_file(files, cwd, &path, None);
}

fn push_absolute_file(
    files: &mut Vec<KnowledgeFile>,
    cwd: &Path,
    path: &Path,
    prefix: Option<&str>,
) {
    if !path.is_file() {
        return;
    }
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let label = match prefix {
        Some(prefix) => format!("{}:{}", prefix, relative_label(cwd, path)),
        None => relative_label(cwd, path),
    };
    files.push(KnowledgeFile {
        label,
        content: trim_multiline(&content, MAX_FILE_CHARS),
    });
}

fn format_knowledge_files(title: &str, files: &[KnowledgeFile]) -> String {
    let mut out = format!("[{title}]");
    for file in files {
        out.push_str(&format!(
            "\n\nSource: {}\n{}",
            file.label,
            file.content.trim()
        ));
    }
    out
}

fn format_skill_index(skills: &[SkillSummary]) -> String {
    let mut out = String::from(
        "[Auto-discovered Holmes skills]\nHolmes may apply these skills automatically when the task matches. Load the full skill file only when it is relevant.",
    );
    for skill in skills {
        out.push_str(&format!(
            "\n- {}: {} ({})",
            skill.name, skill.description, skill.path
        ));
    }
    out
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

fn relative_label(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn enforce_total_limit(content: String) -> String {
    trim_multiline(&content, MAX_TOTAL_CONTEXT_CHARS)
}

fn trim_multiline(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.trim().to_string();
    }
    let mut out = content.chars().take(max_chars).collect::<String>();
    out.push_str("\n[truncated]");
    out.trim().to_string()
}

fn trim_inline(content: &str, max_chars: usize) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        normalized
    } else {
        let mut out = normalized.chars().take(max_chars).collect::<String>();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn build_system_prompt_includes_native_capabilities_without_project_files() {
        let cwd = temp_project_dir();
        let prompt = build_system_prompt("base", &HolmesConfig::default(), &cwd, SessionMode::Pentest);

        assert!(prompt.contains("base"));
        assert!(prompt.contains("Holmes native capabilities - always on"));
        assert!(prompt.contains("Maintain the case state automatically"));

        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn pentest_mode_injects_pentest_methodology() {
        let cwd = temp_project_dir();
        let prompt = build_system_prompt("base", &HolmesConfig::default(), &cwd, SessionMode::Pentest);

        // 三阶段
        assert!(prompt.contains("Discovery"));
        assert!(prompt.contains("Attack"));
        assert!(prompt.contains("Audit"));
        // 12 维度威胁建模标记
        assert!(prompt.contains("12 维度威胁建模"));
        // 关键 banned pattern
        assert!(prompt.contains("200 ≠ 漏洞确认"));
        assert!(prompt.contains("unruled_out"));
        // 完整资料指针
        assert!(prompt.contains("skills/pentest-lyan/SKILL.md"));

        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn non_pentest_mode_skips_pentest_methodology() {
        let cwd = temp_project_dir();
        for mode in [
            SessionMode::CodeAudit,
            SessionMode::Reverse,
            SessionMode::SecurityResearch,
            SessionMode::Mixed,
        ] {
            let prompt = build_system_prompt("base", &HolmesConfig::default(), &cwd, mode);
            assert!(!prompt.contains("Pentest methodology - default"));
            assert!(!prompt.contains("12 维度威胁建模"));
        }
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn build_system_prompt_auto_loads_rules_and_skill_index() {
        let cwd = temp_project_dir();
        fs::write(
            cwd.join("HOLMES.md"),
            "# Project Holmes\nUse evidence first.",
        )
        .unwrap();
        fs::create_dir_all(cwd.join(".holmes/rules")).unwrap();
        fs::write(
            cwd.join(".holmes/rules/scope.md"),
            "# Scope\nOnly test owned systems.",
        )
        .unwrap();
        fs::create_dir_all(cwd.join(".holmes/skills/recon")).unwrap();
        fs::write(
            cwd.join(".holmes/skills/recon/SKILL.md"),
            "---\nname: recon\n
description: Build a target map before validation.\n---\n# Recon\n",
        )
        .unwrap();

        let prompt = build_system_prompt("base", &HolmesConfig::default(), &cwd, SessionMode::Pentest);

        assert!(prompt.contains("Auto-loaded Holmes knowledge"));
        assert!(prompt.contains("Source: HOLMES.md"));
        assert!(prompt.contains("Use evidence first."));
        assert!(prompt.contains("Source: rule:.holmes/rules/scope.md"));
        assert!(prompt.contains("Only test owned systems."));
        assert!(prompt.contains("Auto-discovered Holmes skills"));
        assert!(prompt.contains("recon: Build a target map before validation."));

        let _ = fs::remove_dir_all(cwd);
    }

    fn temp_project_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("holmes-project-knowledge-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
