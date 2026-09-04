//! Curated MCP server catalog — lean port of hermes' `optional-mcps/`
//! manifests (`GET /api/mcp/catalog` + `POST /api/mcp/catalog/install`).
//! Entries are well-known reference servers; installing one appends a
//! `[[mcp.servers]]` entry to config.toml (restart or `/reload-mcp`
//! applies it).

/// One environment variable an entry needs from the user.
#[derive(Debug, Clone, Copy)]
pub struct CatalogEnvVar {
    pub name: &'static str,
    pub prompt: &'static str,
}

/// A catalog entry (stdio launch recipe).
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub required_env: &'static [CatalogEnvVar],
}

/// The curated catalog (hermes `optional-mcps` parity, lean selection).
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        name: "filesystem",
        description:
            "Read/write/list files under allowed directories (reference filesystem server).",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-filesystem", "."],
        required_env: &[],
    },
    CatalogEntry {
        name: "fetch",
        description: "Fetch web pages and convert them to markdown for the model.",
        command: "uvx",
        args: &["mcp-server-fetch"],
        required_env: &[],
    },
    CatalogEntry {
        name: "memory",
        description: "Persistent knowledge-graph memory (entity/relation store).",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-memory"],
        required_env: &[],
    },
    CatalogEntry {
        name: "time",
        description: "Current time and timezone conversions.",
        command: "uvx",
        args: &["mcp-server-time"],
        required_env: &[],
    },
    CatalogEntry {
        name: "everything",
        description: "Reference/test server exercising every MCP feature (demo).",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-everything"],
        required_env: &[],
    },
    CatalogEntry {
        name: "sequential-thinking",
        description: "Structured step-by-step reasoning tool.",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
        required_env: &[],
    },
    CatalogEntry {
        name: "github",
        description: "Repositories, issues, pull requests and file access on GitHub.",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-github"],
        required_env: &[CatalogEnvVar {
            name: "GITHUB_TOKEN",
            prompt: "GitHub personal access token",
        }],
    },
    CatalogEntry {
        name: "gitlab",
        description: "Merge requests, pipelines and repository access on GitLab.",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-gitlab"],
        required_env: &[CatalogEnvVar {
            name: "GITLAB_PERSONAL_ACCESS_TOKEN",
            prompt: "GitLab personal access token",
        }],
    },
    CatalogEntry {
        name: "brave-search",
        description: "Web + local search via the Brave Search API.",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-brave-search"],
        required_env: &[CatalogEnvVar {
            name: "BRAVE_API_KEY",
            prompt: "Brave Search API key",
        }],
    },
    CatalogEntry {
        name: "puppeteer",
        description: "Headless Chrome automation: navigate, screenshot, click, fill.",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-puppeteer"],
        required_env: &[],
    },
];

/// Look one entry up by name.
pub fn get_entry(name: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|entry| entry.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_entries_are_well_formed() {
        assert!(!CATALOG.is_empty());
        let mut seen = std::collections::HashSet::new();
        for entry in CATALOG {
            assert!(!entry.name.is_empty());
            assert!(!entry.description.is_empty());
            assert!(!entry.command.is_empty());
            assert!(
                seen.insert(entry.name),
                "duplicate catalog entry {}",
                entry.name
            );
        }
        assert!(get_entry("filesystem").is_some());
        assert!(get_entry("no-such").is_none());
    }
}
