use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Manage subscription accounts without printing credentials"
)]
struct Cli {
    #[arg(long, global = true, default_value = "http://127.0.0.1:8787")]
    url: String,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Exit with an error unless the service answers its health check.
    Health,
    Status,
    Accounts,
    Models,
    Quotas {
        #[arg(long)]
        refresh: bool,
    },
    Import {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    Remove {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Login {
        provider: String,
    },
    Complete {
        id: String,
        #[arg(long)]
        callback_file: PathBuf,
    },
    /// Save settings, disabled models and every account, tokens included, to
    /// a file only its owner can read. Refresh tokens rotate: restore it in
    /// one douane and remove the accounts from this one.
    Export {
        #[arg(long)]
        output: PathBuf,
    },
    /// Restore an export: accounts are added, or updated when already known.
    Restore {
        path: PathBuf,
        /// Also apply the export's settings and disabled models.
        #[arg(long)]
        settings: bool,
    },
    /// Custom API endpoints: one route (model prefix) per upstream, its keys
    /// rotating. Keys come from a file or stdin, never from arguments.
    #[command(subcommand)]
    Endpoint(Endpoint),
    /// Client keys: once one client exists, every call needs a key. Each
    /// client reaches the accounts its access allows and holds its share of
    /// each provider's quota windows.
    #[command(subcommand)]
    Client(ClientCommand),
}
#[derive(Subcommand)]
enum ClientCommand {
    /// List clients, their access, shares and attributed usage.
    List,
    /// Create a client: `douanectl client add Famille --pools claude
    /// --share claude=25 --key-file famille.key`.
    Add {
        name: String,
        #[command(flatten)]
        rules: ClientRules,
        /// Write the new key to this file instead of printing it.
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
    /// Change a client's name, access or shares, or replace its key.
    Set {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        rules: ClientRules,
        /// Replace the key; the old one stops working at once.
        #[arg(long)]
        rotate_key: bool,
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
    Remove {
        id: String,
    },
}
#[derive(clap::Args)]
struct ClientRules {
    /// Allow every account, accounts connected later included.
    #[arg(long, conflicts_with_all = ["pools", "accounts"])]
    all: bool,
    /// Comma-separated providers or routes whose every account is allowed.
    #[arg(long, value_delimiter = ',')]
    pools: Option<Vec<String>>,
    /// Comma-separated account ids allowed beyond the pools.
    #[arg(long, value_delimiter = ',')]
    accounts: Option<Vec<String>>,
    /// Comma-separated `pool=percent` shares; an empty value removes them.
    #[arg(long, value_delimiter = ',')]
    share: Option<Vec<String>>,
    /// Comma-separated `account_id=percent` overrides; an empty value removes them.
    #[arg(long, value_delimiter = ',')]
    account_share: Option<Vec<String>>,
}

impl ClientRules {
    fn patch(self, patch: &mut Value) -> Result<()> {
        if self.all {
            patch["access"] = json!({"all": true});
        } else if self.pools.is_some() || self.accounts.is_some() {
            let list = |v: Option<Vec<String>>| {
                v.unwrap_or_default()
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            };
            patch["access"] =
                json!({"all": false, "pools": list(self.pools), "accounts": list(self.accounts)});
        }
        for (field, shares) in [
            ("shares", self.share),
            ("account_shares", self.account_share),
        ] {
            let Some(shares) = shares else { continue };
            let mut map = serde_json::Map::new();
            for share in shares.iter().filter(|s| !s.is_empty()) {
                let (pool, percent) = share
                    .split_once('=')
                    .with_context(|| format!("{share}: write a share as pool=percent"))?;
                let percent: f64 = percent
                    .trim_end_matches('%')
                    .parse()
                    .with_context(|| format!("{share}: the percent is not a number"))?;
                map.insert(pool.to_owned(), json!(percent));
            }
            patch[field] = Value::Object(map);
        }
        Ok(())
    }
}

/// Prints a new key, or writes it to a private file and says where.
fn deliver_key(mut value: Value, key_file: Option<PathBuf>) -> Result<Value> {
    if let (Some(path), Some(key)) = (key_file, value["key"].as_str().map(str::to_owned)) {
        write_private(&path, key.as_bytes())?;
        value["key"] = json!(format!("written to {}", path.display()));
    }
    Ok(value)
}

async fn client_command(
    client: &reqwest::Client,
    root: &str,
    command: ClientCommand,
) -> Result<Value> {
    match command {
        ClientCommand::List => send(client.get(format!("{root}/admin/api/clients"))).await,
        ClientCommand::Add {
            name,
            rules,
            key_file,
        } => {
            let mut body = json!({ "name": name });
            rules.patch(&mut body)?;
            let value = send(client.post(format!("{root}/admin/api/clients")).json(&body)).await?;
            deliver_key(value, key_file)
        }
        ClientCommand::Set {
            id,
            name,
            rules,
            rotate_key,
            key_file,
        } => {
            let mut body = json!({});
            if let Some(name) = name {
                body["name"] = json!(name);
            }
            if rotate_key {
                body["rotate_key"] = json!(true);
            }
            rules.patch(&mut body)?;
            if body.as_object().is_some_and(|p| p.is_empty()) {
                bail!(
                    "give --name, --all, --pools, --accounts, --share, --account-share or --rotate-key"
                );
            }
            let value = send(
                client
                    .patch(format!("{root}/admin/api/clients/{id}"))
                    .json(&body),
            )
            .await?;
            deliver_key(value, key_file)
        }
        ClientCommand::Remove { id } => {
            let response = client.delete(format!("{root}/admin/api/clients/{id}"));
            let response = match std::env::var("DOUANE_MANAGEMENT_TOKEN") {
                Ok(token) => response.bearer_auth(token),
                Err(_) => response,
            }
            .send()
            .await
            .context("cannot reach douane")?;
            if !response.status().is_success() {
                bail!("douane returned {}", response.status());
            }
            Ok(json!({ "removed": id }))
        }
    }
}
#[derive(Subcommand)]
enum Endpoint {
    /// List endpoints with their route, address, format and models.
    List,
    /// Add a key for a route: `douanectl endpoint add deepseek
    /// https://api.deepseek.com/v1 --key-file key.txt`. Adding a second key
    /// to a route makes them rotate.
    Add {
        route: String,
        base_url: String,
        /// File holding the API key; `-` reads it from stdin.
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// chat, responses or messages; absent forwards the client's format.
        #[arg(long)]
        dialect: Option<String>,
        /// Comma-separated model ids to serve; absent serves every listed model.
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,
    },
    /// Change every key of a route (or one account id) in place.
    Set {
        route_or_id: String,
        #[arg(long)]
        base_url: Option<String>,
        /// chat, responses or messages; `client` forwards the client's format.
        #[arg(long)]
        dialect: Option<String>,
        /// Comma-separated model ids; an empty value serves every model.
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,
        /// Rename the route.
        #[arg(long)]
        route: Option<String>,
        /// Replace the key; needs a route with a single key, or an id.
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
    /// Remove every key of a route, or one account id.
    Remove { route_or_id: String },
}

async fn read_key(path: &std::path::Path) -> Result<String> {
    let raw = if path.as_os_str() == "-" {
        let mut buffer = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
            .context("reading the key from stdin")?;
        buffer
    } else {
        tokio::fs::read_to_string(path)
            .await
            .context("reading the key file")?
    };
    let key = raw.trim().to_owned();
    if key.is_empty() {
        bail!("the key is empty");
    }
    Ok(key)
}

async fn send(request: reqwest::RequestBuilder) -> Result<Value> {
    let request = match std::env::var("DOUANE_MANAGEMENT_TOKEN") {
        Ok(token) => request.bearer_auth(token),
        Err(_) => request,
    };
    let response = request.send().await.context("cannot reach douane")?;
    let status = response.status();
    let value: Value = response.json().await.context("invalid server response")?;
    if !status.is_success() {
        bail!(
            "douane returned {status}: {}",
            value["error"]["message"]
                .as_str()
                .or_else(|| value.pointer("/errors/0/error").and_then(Value::as_str))
                .unwrap_or("request failed")
        );
    }
    Ok(value)
}

/// Accounts a route or an id names, custom endpoints only.
async fn endpoint_accounts(
    client: &reqwest::Client,
    root: &str,
    route_or_id: &str,
) -> Result<Vec<Value>> {
    let listing = send(client.get(format!("{root}/admin/api/accounts"))).await?;
    let found: Vec<Value> = listing["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["provider"] == "openai_compatible")
        .filter(|a| a["route"] == route_or_id || a["id"] == route_or_id)
        .cloned()
        .collect();
    if found.is_empty() {
        bail!("no custom endpoint has the route or id {route_or_id}");
    }
    Ok(found)
}

async fn endpoint(client: &reqwest::Client, root: &str, command: Endpoint) -> Result<Value> {
    match command {
        Endpoint::List => {
            let listing = send(client.get(format!("{root}/admin/api/accounts"))).await?;
            let rows: Vec<Value> = listing["accounts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["provider"] == "openai_compatible")
                .map(|a| {
                    json!({"id":a["id"],"name":a["name"],"route":a["route"],"base_url":a["base_url"],
                        "dialect":a["dialect"],"models":a["models"],"enabled":a["enabled"],"status":a["status"]})
                })
                .collect();
            Ok(json!({ "endpoints": rows }))
        }
        Endpoint::Add {
            route,
            base_url,
            key_file,
            name,
            dialect,
            models,
        } => {
            let key = read_key(&key_file).await?;
            let mut content =
                json!({"type":"openai_compatible","route":route,"base_url":base_url,"api_key":key});
            if let Some(dialect) = dialect {
                content["dialect"] = json!(dialect);
            }
            if let Some(models) = models {
                content["models"] = json!(models);
            }
            send(
                client
                    .post(format!("{root}/admin/api/accounts/import"))
                    .json(&json!({"files":[{"name":name,"content":content}]})),
            )
            .await
        }
        Endpoint::Set {
            route_or_id,
            base_url,
            dialect,
            models,
            route,
            key_file,
        } => {
            let accounts = endpoint_accounts(client, root, &route_or_id).await?;
            let mut patch = json!({});
            if let Some(url) = base_url {
                patch["base_url"] = json!(url);
            }
            if let Some(dialect) = dialect {
                patch["dialect"] = if dialect == "client" {
                    Value::Null
                } else {
                    json!(dialect)
                };
            }
            if let Some(models) = models {
                patch["models"] = json!(
                    models
                        .into_iter()
                        .filter(|m| !m.is_empty())
                        .collect::<Vec<_>>()
                );
            }
            if let Some(route) = route {
                patch["route"] = json!(route);
            }
            if let Some(path) = key_file {
                if accounts.len() != 1 {
                    bail!(
                        "{route_or_id} has {} keys; replace one by its account id",
                        accounts.len()
                    );
                }
                patch["api_key"] = json!(read_key(&path).await?);
            }
            if patch.as_object().is_some_and(|p| p.is_empty()) {
                bail!("give --base-url, --dialect, --models, --route or --key-file");
            }
            let mut changed = vec![];
            for account in accounts {
                let id = account["id"].as_str().unwrap_or_default();
                let result = send(
                    client
                        .patch(format!("{root}/admin/api/accounts/{id}"))
                        .json(&patch),
                )
                .await?;
                changed.push(result["account"].clone());
            }
            Ok(json!({ "updated": changed }))
        }
        Endpoint::Remove { route_or_id } => {
            let accounts = endpoint_accounts(client, root, &route_or_id).await?;
            let mut removed = vec![];
            for account in accounts {
                let id = account["id"].as_str().unwrap_or_default();
                send(client.delete(format!("{root}/admin/api/accounts/{id}"))).await?;
                removed.push(json!(id));
            }
            Ok(json!({ "removed": removed }))
        }
    }
}
/// Writes an export readable by its owner only where the OS has such modes;
/// on Windows the file inherits the folder's access list.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        // An existing file keeps its mode through open; tighten it first.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting {}", path.display()))?;
        }
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    file.sync_all()?;
    Ok(())
}

async fn export(client: &reqwest::Client, root: &str, output: &std::path::Path) -> Result<Value> {
    let bundle = send(client.get(format!("{root}/admin/api/export"))).await?;
    write_private(output, &serde_json::to_vec_pretty(&bundle)?)?;
    Ok(json!({
        "exported": bundle["accounts"].as_array().map_or(0, Vec::len),
        "output": output.display().to_string(),
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let root = cli.url.trim_end_matches('/');
    let command = match cli.command {
        Command::Endpoint(command) => {
            let value = endpoint(&client, root, command).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }
        Command::Client(command) => {
            let value = client_command(&client, root, command).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }
        Command::Export { output } => {
            let value = export(&client, root, &output).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }
        command => command,
    };
    let request = match command {
        Command::Health => client.get(format!("{root}/health")),
        Command::Status => client.get(format!("{root}/admin/api/summary")),
        Command::Accounts => client.get(format!("{root}/admin/api/accounts")),
        Command::Models => client.get(format!("{root}/admin/api/models")),
        Command::Quotas { refresh: false } => client.get(format!("{root}/admin/api/quotas")),
        Command::Quotas { refresh: true } => client
            .post(format!("{root}/admin/api/quotas/refresh"))
            .json(&json!({})),
        Command::Import { path, name } => {
            let content: Value =
                serde_json::from_slice(&tokio::fs::read(&path).await.context("reading auth file")?)
                    .context("invalid auth JSON")?;
            client
                .post(format!("{root}/admin/api/accounts/import"))
                .json(&json!({"files":[{"name":name,"content":content}]}))
        }
        Command::Remove { id } => client.delete(format!("{root}/admin/api/accounts/{id}")),
        Command::Enable { id } => client
            .patch(format!("{root}/admin/api/accounts/{id}"))
            .json(&json!({"enabled":true})),
        Command::Disable { id } => client
            .patch(format!("{root}/admin/api/accounts/{id}"))
            .json(&json!({"enabled":false})),
        Command::Login { provider } => client
            .post(format!("{root}/admin/api/oauth/start"))
            .json(&json!({"provider":provider})),
        Command::Complete { id, callback_file } => {
            let callback = tokio::fs::read_to_string(callback_file).await?;
            client
                .post(format!("{root}/admin/api/oauth/callback"))
                .json(&json!({"id":id,"code":callback.trim(),"state":""}))
        }
        Command::Restore { path, settings } => {
            let bundle: Value = serde_json::from_slice(
                &tokio::fs::read(&path).await.context("reading the export")?,
            )
            .context("the export is not JSON")?;
            client
                .post(format!("{root}/admin/api/restore"))
                .json(&json!({"bundle":bundle,"settings":settings}))
        }
        Command::Endpoint(_) | Command::Client(_) | Command::Export { .. } => {
            unreachable!("handled above")
        }
    };
    let value = send(request).await?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_account_shares_can_be_added_updated_and_cleared() {
        for (arguments, expected) in [
            (
                vec![
                    "douanectl",
                    "client",
                    "add",
                    "family",
                    "--share",
                    "claude=95",
                    "--account-share",
                    "abc=10%,def=25",
                ],
                json!({"shares":{"claude":95.0},"account_shares":{"abc":10.0,"def":25.0}}),
            ),
            (
                vec![
                    "douanectl",
                    "client",
                    "set",
                    "id",
                    "--account-share",
                    "abc=20",
                ],
                json!({"account_shares":{"abc":20.0}}),
            ),
            (
                vec![
                    "douanectl",
                    "client",
                    "set",
                    "id",
                    "--account-share",
                    "",
                    "--share",
                    "",
                ],
                json!({"shares":{},"account_shares":{}}),
            ),
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let rules = match cli.command {
                Command::Client(
                    ClientCommand::Add { rules, .. } | ClientCommand::Set { rules, .. },
                ) => rules,
                _ => panic!("expected client rules"),
            };
            let mut patch = json!({});
            rules.patch(&mut patch).unwrap();
            assert_eq!(patch, expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_export_file_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("douanectl-export-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("export.json");
        // An existing readable file is tightened before it receives tokens.
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        super::write_private(&path, b"{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
