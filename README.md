# Jarvis (nom de code)

Agent personnel généraliste en Rust + Wasm : pilote Claude Code, Codex et Gemini via leurs abonnements, s'interconnecte aux sessions T3 Code, gère le quotidien (agenda, rappels, mémoire) et agrège les outils MCP.

Le nom définitif reste à trouver.

Conception : [docs/superpowers/specs/2026-11-16-jarvis-design.md](docs/superpowers/specs/2026-11-16-jarvis-design.md)

## Démarrer (étape 1a : le hub MCP)

Le hub expose en MCP la mémoire, l'agenda et les rappels CalDAV, et les outils d'autres serveurs MCP (MCPHub…) derrière `search_tools`.

1. Copier `jarvis.example.toml` et l'adapter. Les secrets ne vont jamais dans le fichier : `{ file = "…" }` ou `{ env = "…" }`.
2. Créer le jeton (32 caractères minimum) : `openssl rand -hex 32 > jarvis_token`.
3. Vérifier : `jarvis check --config jarvis.toml`.

En natif :

```sh
cargo run --release -p jarvis -- serve --config jarvis.toml
```

En Docker (dans le conteneur, `listen = "0.0.0.0:8484"`, `dir = "/data"`) :

```sh
docker build -t jarvis .
docker run -d -p 8484:8484 -v ./jarvis.toml:/config/jarvis.toml:ro \
  -v ./jarvis_token:/run/secrets/jarvis_token:ro -v jarvis-data:/data jarvis
```

Déclarer le hub dans Claude Code :

```sh
claude mcp add --transport http jarvis http://127.0.0.1:8484/mcp \
  --header "Authorization: Bearer $(cat jarvis_token)"
```

Toute autre application MCP (T3 Code, Codex, MCPHub…) se branche de la même façon : URL `/mcp`, en-tête `Authorization: Bearer <jeton>`.
