use fronde_agent::acp::AgentSpec;
use fronde_hub::config::{ComponentConfig, Config};
use fronde_wasm::{Checks, Credential, prepare};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn dist(file: &str) -> PathBuf {
    let p = workspace_root().join("components/dist").join(file);
    assert!(
        p.exists(),
        "{} is missing: run `mise run components`",
        p.display()
    );
    p
}

/// Whether an attribute is `#[cfg(test)]` or `#[cfg(all(test, ...))]`: what only builds for tests.
/// `cfg(not(test))`, `cfg(any(test, ...))` and feature gates are production code.
fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let Ok(meta) = attr.parse_args::<syn::Meta>() else {
            return false;
        };
        match meta {
            syn::Meta::Path(p) => p.is_ident("test"),
            syn::Meta::List(list) if list.path.is_ident("all") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|all| {
                    all.iter()
                        .any(|m| matches!(m, syn::Meta::Path(p) if p.is_ident("test")))
                }),
            _ => false,
        }
    })
}

/// Every line where an identifier starting with `expose_secret` appears in `tokens`, macro
/// bodies included: a method call, `ExposeSecret::expose_secret`, `expose_secret_mut`.
fn expose_lines(tokens: proc_macro2::TokenStream, out: &mut Vec<usize>) {
    for tree in tokens {
        match tree {
            proc_macro2::TokenTree::Ident(ident) => {
                if ident.to_string().starts_with("expose_secret") {
                    out.push(ident.span().start().line);
                }
            }
            proc_macro2::TokenTree::Group(group) => expose_lines(group.stream(), out),
            _ => {}
        }
    }
}

/// Walks the items that build outside tests, down into inline modules, impls and traits.
fn production_expose_lines(items: &[syn::Item], out: &mut Vec<usize>) {
    use quote::ToTokens;
    for item in items {
        match item {
            syn::Item::Mod(m) if is_test_only(&m.attrs) => {}
            syn::Item::Mod(syn::ItemMod {
                content: Some((_, items)),
                ..
            }) => production_expose_lines(items, out),
            syn::Item::Impl(i) if !is_test_only(&i.attrs) => {
                for item in &i.items {
                    let attrs = match item {
                        syn::ImplItem::Const(c) => &c.attrs,
                        syn::ImplItem::Fn(f) => &f.attrs,
                        syn::ImplItem::Type(t) => &t.attrs,
                        syn::ImplItem::Macro(m) => &m.attrs,
                        _ => &Vec::new(),
                    };
                    if !is_test_only(attrs) {
                        expose_lines(item.to_token_stream(), out);
                    }
                }
            }
            syn::Item::Trait(t) if !is_test_only(&t.attrs) => {
                for item in &t.items {
                    let attrs = match item {
                        syn::TraitItem::Const(c) => &c.attrs,
                        syn::TraitItem::Fn(f) => &f.attrs,
                        syn::TraitItem::Type(t) => &t.attrs,
                        syn::TraitItem::Macro(m) => &m.attrs,
                        _ => &Vec::new(),
                    };
                    if !is_test_only(attrs) {
                        expose_lines(item.to_token_stream(), out);
                    }
                }
            }
            syn::Item::Impl(_) | syn::Item::Trait(_) => {}
            other => {
                let attrs: &[syn::Attribute] = match other {
                    syn::Item::Const(i) => &i.attrs,
                    syn::Item::Enum(i) => &i.attrs,
                    syn::Item::ExternCrate(i) => &i.attrs,
                    syn::Item::Fn(i) => &i.attrs,
                    syn::Item::ForeignMod(i) => &i.attrs,
                    syn::Item::Macro(i) => &i.attrs,
                    syn::Item::Static(i) => &i.attrs,
                    syn::Item::Struct(i) => &i.attrs,
                    syn::Item::TraitAlias(i) => &i.attrs,
                    syn::Item::Type(i) => &i.attrs,
                    syn::Item::Union(i) => &i.attrs,
                    syn::Item::Use(i) => &i.attrs,
                    _ => &[],
                };
                if !is_test_only(attrs) {
                    expose_lines(other.to_token_stream(), out);
                }
            }
        }
    }
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// `expose_secret` is allowed only at these sites, each justified. A new use is added here on
/// purpose and reviewed (AGENTS.md).
const ALLOWED: &[(&str, usize)] = &[
    // The hub's token: in the agent's environment for a spawned agent (its MCP header names the
    // variable), in the header itself for an agent in this process; and the agent's Claude
    // token, in its environment.
    ("crates/agent/src/acp.rs", 3),
    // The master key: its bytes given to the cipher, and its one line written to the new key
    // file.
    ("crates/db/src/master_key.rs", 2),
    // A stored secret's value, sealed by the cipher.
    ("crates/db/src/secrets.rs", 1),
    // The upstream MCP token handed to rmcp's transport.
    ("crates/hub/src/upstream.rs", 1),
    // `check_token`'s length, `check_password`'s emptiness, and `same_secret`'s digest, which
    // compares the `/mcp` token and the web password in constant time.
    ("crates/server/src/lib.rs", 4),
    // The gate: the Basic and Bearer headers built for each request, and the Bearer token's
    // bytes checked once when the grants are made.
    ("crates/wasm/src/egress.rs", 3),
];

#[test]
fn expose_secret_call_sites() {
    let root = workspace_root();
    let mut files = Vec::new();
    for parent in ["crates", "components"] {
        let entries = std::fs::read_dir(root.join(parent)).unwrap();
        for entry in entries.flatten() {
            let src = entry.path().join("src");
            if src.is_dir() {
                collect_rs_files(&src, &mut files);
            }
        }
    }
    files.sort();
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("crates/wasm/src/egress.rs"))
            && files.iter().any(|f| f.starts_with(root.join("components"))),
        "the walk missed the sources: {files:?}"
    );

    let mut found: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let parsed = syn::parse_file(&text)
            .unwrap_or_else(|e| panic!("cannot parse {}: {e}", file.display()));
        if is_test_only(&parsed.attrs) {
            continue;
        }
        let mut lines = Vec::new();
        production_expose_lines(&parsed.items, &mut lines);
        if !lines.is_empty() {
            let rel = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(rel, lines);
        }
    }

    let actual: BTreeMap<&str, usize> = found
        .iter()
        .map(|(file, lines)| (file.as_str(), lines.len()))
        .collect();
    let expected: BTreeMap<&str, usize> = ALLOWED.iter().copied().collect();
    assert_eq!(
        actual, expected,
        "expose_secret call sites changed (file: lines): {found:#?}"
    );
}

/// The walk sees what a line search missed, and skips only what builds for tests alone.
#[test]
fn expose_secret_walk_sees_macros_and_paths() {
    let source = r#"
        use secrecy::ExposeSecret;
        fn a(s: &S) { tracing::info!("{}", s.expose_secret()); }
        fn b(s: &S) -> &str { ExposeSecret::expose_secret(s) }
        fn c(s: &mut S) { s.expose_secret_mut(); }
        #[cfg(not(test))]
        fn d(s: &S) { s.expose_secret(); }
        #[cfg(feature = "x")]
        fn e(s: &S) { s.expose_secret(); }
        impl T {
            fn f(&self) { self.0.expose_secret(); }
            #[cfg(test)]
            fn g(&self) { self.0.expose_secret(); }
        }
        #[cfg(test)]
        mod tests { fn h(s: &S) { s.expose_secret(); } }
        #[cfg(all(test, unix))]
        mod unix_tests { fn i(s: &S) { s.expose_secret(); } }
        mod inner {
            fn j(s: &S) { s.expose_secret(); }
            #[cfg(test)]
            fn k(s: &S) { s.expose_secret(); }
        }
    "#;
    let parsed = syn::parse_file(source).unwrap();
    let mut lines = Vec::new();
    production_expose_lines(&parsed.items, &mut lines);
    assert_eq!(lines, [3, 4, 5, 7, 9, 11, 20]);
}

#[test]
fn no_secret_in_debug() {
    let dir = tempfile::tempdir().unwrap();
    let mcp_canary = "canary-mcp-token-0123456789abcdef0123456789";
    let web_canary = "canary-web-password-9f8e7d6c";
    let upstream_canary = "canary-upstream-token-5a4b3c2d";
    let caldav_canary = "canary-caldav-password-1e2f3a4b";
    let bearer_canary = "canary-bearer-token-7c8d9e0f";
    let agent_canary = "canary-agent-hub-token-3b4c5d6e";

    let token_file = dir.path().join("mcp_token");
    let web_file = dir.path().join("web_pass");
    let upstream_file = dir.path().join("upstream_tok");
    let caldav_file = dir.path().join("caldav_pass");
    fronde_hub::private::write(&token_file, mcp_canary).unwrap();
    fronde_hub::private::write(&web_file, web_canary).unwrap();
    fronde_hub::private::write(&upstream_file, upstream_canary).unwrap();
    fronde_hub::private::write(&caldav_file, caldav_canary).unwrap();

    let wasm = dist("calendar.wasm");
    let manifest = dist("calendar.toml");
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n\
         [data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
         [web]\npassword = {{ file = {:?} }}\n\
         [[sources]]\nkind = \"mcp\"\nname = \"mcphub\"\nurl = \"https://mcphub.example/mcp\"\ntoken = {{ file = {:?} }}\n\
         [[sources]]\nkind = \"component\"\nname = \"calendar\"\npath = {:?}\nmanifest = {:?}\n\
         allow = [\"https://cloud.example\"]\n\
         [sources.settings]\nevents_url = \"https://cloud.example/e/\"\nreminders_url = \"https://cloud.example/r/\"\n\
         [[sources.credentials]]\norigin = \"https://cloud.example\"\n\
         basic = {{ username = \"alix\", password = {{ file = {:?} }} }}\n",
        token_file.display().to_string(),
        dir.path().join("data").display().to_string(),
        web_file.display().to_string(),
        upstream_file.display().to_string(),
        wasm.display().to_string(),
        manifest.display().to_string(),
        caldav_file.display().to_string(),
    );
    let config_path = dir.path().join("fronde.toml");
    std::fs::write(&config_path, &toml).unwrap();
    let config: Config = fronde_hub::config::load(&config_path).unwrap();

    let component_cfg: ComponentConfig = toml::from_str(&format!(
        "name = \"calendar\"\npath = {:?}\nmanifest = {:?}\n\
         allow = [\"https://cloud.example\"]\n\
         [settings]\nevents_url = \"https://cloud.example/e/\"\nreminders_url = \"https://cloud.example/r/\"\n\
         [[credentials]]\norigin = \"https://cloud.example\"\n\
         basic = {{ username = \"alix\", password = {{ file = {:?} }} }}\n",
        wasm.display().to_string(),
        manifest.display().to_string(),
        caldav_file.display().to_string(),
    ))
    .unwrap();
    let prepared = prepare(&component_cfg, &Checks::of(&config)).unwrap();
    let spec = prepared.spec();

    let basic = Credential::Basic {
        username: "alix".into(),
        password: caldav_canary.into(),
    };
    let bearer = Credential::Bearer(bearer_canary.into());
    let agent_spec = AgentSpec {
        thread: fronde_agent::threads::ThreadId::MAIN,
        provider: &fronde_agent::provider::Provider::CLAUDE,
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: dir.path().to_path_buf(),
        home: dir.path().join("home"),
        hub_url: "http://127.0.0.1:8484/mcp".into(),
        hub_token: agent_canary.into(),
        oauth_token: None,
        system_prompt: "You are Fronde.".into(),
        builtin_tools: Vec::new(),
        usage: None,
    };

    let all_canaries = [
        mcp_canary,
        web_canary,
        upstream_canary,
        caldav_canary,
        bearer_canary,
        agent_canary,
    ];
    let formatted = [
        ("Config compact", format!("{config:?}")),
        ("Config pretty", format!("{config:#?}")),
        ("Prepared compact", format!("{prepared:?}")),
        ("Prepared pretty", format!("{prepared:#?}")),
        ("ComponentSpec compact", format!("{spec:?}")),
        ("ComponentSpec pretty", format!("{spec:#?}")),
        ("Credential::Basic compact", format!("{basic:?}")),
        ("Credential::Basic pretty", format!("{basic:#?}")),
        ("Credential::Bearer compact", format!("{bearer:?}")),
        ("Credential::Bearer pretty", format!("{bearer:#?}")),
        ("AgentSpec compact", format!("{agent_spec:?}")),
        ("AgentSpec pretty", format!("{agent_spec:#?}")),
    ];
    for (label, text) in &formatted {
        for canary in all_canaries {
            assert!(!text.contains(canary), "{label} leaked {canary}: {text}");
        }
    }
    assert!(format!("{agent_spec:?}").contains("[REDACTED]"));
    assert!(format!("{agent_spec:#?}").contains("[REDACTED]"));
}
