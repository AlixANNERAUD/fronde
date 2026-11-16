# Jarvis (nom de code) — document de conception

> Date : 2026-11-16. Statut : brouillon à valider.
> « Jarvis » est un nom de code provisoire. **[Claude]** marque un choix de Claude à vérifier par Alix ;
> le reste vient d'Alix ou a été validé dans la conversation.

## Principe directeur

**Le plus simple et le plus élégant possible.** On n'ajoute un mécanisme que lorsque son absence fait mal.
Quand une version simple existe, on la prend, et on garde seulement une frontière propre dans le code pour
pouvoir l'enrichir plus tard.

## 1. Intention

Un **T3 Code généraliste, en Rust + WebAssembly** : il pilote des agents (Claude Code, Codex, Gemini/Antigravity)
sur les abonnements de l'utilisateur, mais au service de **tout** (vie quotidienne, famille, maison, projets),
pas seulement du code. Pour le code, il **s'appuie sur les T3 Code** qui tournent sur les machines de
l'utilisateur, au lieu de les refaire.

Au-dessus vit **Jarvis**, un agent toujours allumé : on lui parle depuis la web app, il gère agenda et rappels,
se construit une mémoire, prévient au bon moment, lance et suit des threads (dont ceux de T3).

**Pourquoi** : Alix a déployé Hermes Agent pour ce rôle et l'a abandonné, trop pénible à configurer. OpenClaw est
trop lourd. Aucun des deux ne gère CalDAV nativement ni ne pilote T3 proprement. Le projet sert d'abord à Alix et
à sa famille, puis sera **publié** avec l'ambition de détrôner OpenClaw et Hermes.

**Positionnement** : sûr par construction (outils Wasm aux droits explicites, clés jamais dans les outils),
abonnements plutôt que clés d'API, un seul fichier de config (IaC ou interface), léger, pensé pour un foyer,
mémoire et outils partagés par tous les agents de l'utilisateur.

## 2. Utilisateurs

- **Administrateur** (Alix, développeur) : configure, pilote ses agents de code via T3, voit tout.
- **Famille, non technique** : utilise **uniquement la web app** pour l'instant. Ne voit ni threads, ni modèles,
  ni outils.

## 3. Objectifs, non-objectifs, anti-objectifs

**Objectifs** : coordonner les T3 d'Alix ; agenda CalDAV et rappels ; mémoire construite à la volée ;
interconnexion MCP dans les deux sens (dont MCPHub) ; outils extensibles en Wasm ; navigateur pilotable par
l'agent ; plusieurs utilisateurs ; natif ou Docker ; autonome sur une machine ou déployé sur une infra avec
accès web de partout ; interface la plus simple possible.

**Non-objectifs** : refaire T3 pour le code (worktrees, diffs, terminaux, PR) ; couvrir autant de messageries
qu'OpenClaw ; une place de marché de plugins ; des fournisseurs par clé d'API ou des modèles locaux ; le
déploiement de l'instance d'Alix (il s'en charge).

**Anti-objectifs** : devenir une usine à gaz ; une config éparpillée ou à double vérité ; un outil qui lit des
secrets ou agit hors de ses droits ; un agent qui s'accorde lui-même des permissions ; accès aux mots de passe ;
une interface qui oblige la famille à comprendre la technique.

## 4. Contraintes

- **Backend Rust ; frontend Leptos** avec **rust-ui** (portage de shadcn, composants copiés dans le projet) et un
  thème propre ; pas Thaw.
- **Abonnements uniquement**, via les clients officiels. Jarvis n'a **pas de boucle d'agent propre** : le client
  CLI la fait tourner, Jarvis fournit contexte, mémoire et outils. Les outils atteignent le modèle par **MCP**.
- Même binaire en **natif ou en Docker**.
- Interconnexion **MCP** avec **MCPHub** et avec les **T3 Code** d'Alix.

## 5. Approches considérées

- **A. Partir de zéro** : sur mesure, tout à écrire.
- **B. Forker ZeroClaw** (Rust, wasmtime) : ses hypothèses (clés d'API, mono-utilisateur, sa propre config)
  contredisent nos différenciants, et la base est déjà grosse.
- **C. Cœur propre + briques et idées reprises** — **choisie**. Les différenciants sont des choix
  d'architecture et doivent être dans le cœur ; on reprend le reste (crates, idées de T3, Hermes, OpenClaw),
  avec vérification des licences.

## 6. Conception

### 6.1 Deux couches

- **Le hub** : outils (natifs, Wasm, MCP amont), recherche d'outils, mémoire, rappels, surveillance de T3,
  navigateur, config, serveur MCP. **Utilisable seul** par n'importe quel agent (T3, Claude Code, Codex, Hermes…).
  C'est la pièce que personne ne propose.
- **Jarvis et la web app**, au-dessus du hub.

Une seule base de code, deux usages publiables : le hub seul, ou le produit complet.

```
 Web app (Leptos) ──► Jarvis ──ACP──► Claude Code / Codex / Gemini   (abonnements)
                        │                    │ MCP
                        ▼                    ▼
                       Hub : outils (natifs · Wasm · MCP amont) · recherche d'outils · mémoire
                             · rappels · surveillance T3 · navigateur · config · serveur /mcp
                        │ MCP                                   ▲ MCP
                        ▼                                       │
                 T3 Code (machines d'Alix), MCPHub, HA…   autres agents (T3, Claude Code…)
```

### 6.2 Concepts

- **Thread** : une conversation avec un agent, via un client CLI, comme dans T3.
- **Jarvis** : l'agent toujours allumé de chaque utilisateur ; répond, ou lance et suit d'autres threads.
- **Espaces** : l'espace privé de chaque utilisateur, plus **un seul espace partagé « famille »** [Claude].

### 6.3 Fournisseurs

- **ACP pour tous** (crate `agent-client-protocol` côté client ; adaptateurs `claude-agent-acp`, `codex-acp`,
  `gemini --acp`). Un protocole natif (ex. le `stream-json` de Claude Code, utilisé par T3) n'est ajouté que si
  ACP manque réellement de quelque chose [Claude].
- Versions des crates et adaptateurs ACP **épinglées** (API très mouvante).
- Identifiants des abonnements fournis par fichier ou variable d'environnement quand le client le permet, plutôt
  que par connexion interactive — à vérifier client par client.
- Isolation [Claude] : en Docker, le conteneur suffit ; en natif, chaque client tourne dans un dossier de travail
  dédié. Pour les threads Jarvis et famille, les outils intégrés des clients (shell, fichiers, web) sont
  désactivés : ils ne voient que les outils du hub. Le shell n'est permis que dans les threads admin.
- Les tâches de fond ont un budget et une priorité plus basse, pour ne pas épuiser le quota qu'Alix utilise pour
  coder [Claude].

### 6.4 Outils

**Trois formes, une règle** :
| Cas | Forme |
|---|---|
| Un bon serveur MCP existe (Home Assistant, Grafana, Paperless, GitHub, T3) | **MCP amont**, tel quel |
| Une API REST sans bon serveur MCP (Mealie, Seerr, Firefly…) | **Composant Wasm** |
| Le cœur (mémoire, rappels, agenda, surveillance T3, navigateur) | **Natif Rust** |

**Wasm, au minimum** : wasmtime, modèle de composants, interface WIT versionnée ; seulement **`wasi-http`** ;
**liste de domaines autorisés** par composant ; **clés injectées par l'hôte** à l'appel HTTP, jamais dans le
composant ; les droits ne se donnent **jamais** par un outil accessible à l'agent. Limites de ressources, accès
fichiers, registre et signature : plus tard, si besoin.

**Guidage des agents, repris de T3** (T3 fait cela très bien ; on copie ses principes) :
- **Outils différés + recherche** : le modèle voit les noms ; `chercher_outils(besoin)` renvoie le schéma
  complet des quelques outils pertinents. Recherche plein texte sur noms, descriptions et exemples.
- **Préfixes cohérents** qui portent le sens (`agenda.*`, `rappels.*`, `memoire.*`, `code.*`, `web.*`,
  `courses.*`).
- **Descriptions télégraphiques et impératives** : des règles d'usage, pas de la documentation.
- **Un court guide par situation** injecté au début : « par défaut, n'utilise pas d'outil » et des règles qui
  départagent les outils proches.
- **Champ `next`** dans les résultats : le conseil arrive au moment où il sert.
- **Attendre plutôt que sonder** : des outils d'attente et des notifications ; jamais de boucle de sondage.
- **Découverte des capacités à la demande** (une fois par thread), plutôt que tout lister dans le prompt.
- **Erreurs qui expliquent** comment corriger.
- **Filtrage par droits** avant tout : un utilisateur ne voit pas les outils qu'il n'a pas le droit d'utiliser.

### 6.5 Interconnexion MCP

- **Client** : le hub consomme des serveurs MCP, dont MCPHub.
- **Serveur** : le hub expose `/mcp` (mémoire, agenda, rappels, outils Wasm, navigateur), protégé par jeton,
  déclarable dans MCPHub. Tous les agents d'Alix accèdent ainsi à sa mémoire et à ses outils.

### 6.6 Interconnexion T3 Code

- Le hub se connecte au **serveur MCP de chaque T3** comme à n'importe quel serveur MCP ; les outils apparaissent
  sous `code.*` (lancer, envoyer, lire, attendre un thread).
- **Surveillance sans modèle** : un module natif détecte fin de thread et demande en attente, notifie
  l'utilisateur (en évitant ses réunions) et relaie sa réponse. Une notification ne coûte aucun appel au modèle.
- Liens directs vers le thread dans T3 pour les diffs et la reprise en main.

### 6.7 Navigateur pilotable

- Doit pouvoir **se connecter à n'importe quel site**, y compris ceux qui bloquent les robots (ex. la CAF).
  Principe : utiliser un **vrai navigateur dans de vraies conditions**, pas camoufler un robot.
  - **Vrai Chrome avec fenêtre**, sur un écran virtuel (Xvfb), plutôt qu'un Chromium headless.
  - **Profil persistant par site** (cookies, historique), activé explicitement par l'utilisateur.
  - **La connexion est faite par l'utilisateur** (FranceConnect, double authentification, captcha) en reprenant
    la main depuis la web app ; l'agent réutilise ensuite la session.
  - Pas de modules de camouflage des traces d'automatisation (fragile, et c'est du contournement).
  - L'adresse IP est hors du périmètre du projet (affaire de déploiement).
  - Aucune garantie de passer partout ; certains sites interdisent l'automatisation dans leurs conditions.
- Piloté par le hub (CDP), exposé en outils `web.*` (ouvrir, naviguer, lire, cliquer/saisir, capture).
- **Le modèle lit la page par un instantané d'accessibilité** (texte structuré), pas par des captures d'écran :
  beaucoup moins de tokens. Les captures ne servent qu'en cas de besoin.
- **L'utilisateur voit la page dans la web app** (flux d'images CDP) et **peut reprendre la main** (connexion,
  captcha), puis rendre la main à l'agent.
- La PWA affiche la page (images CDP) et renvoie clics et saisie : sur téléphone, le navigateur distant passe en
  affichage mobile, et un champ caché capte le clavier. Fichiers téléchargés : récupérés par le hub puis proposés
  dans la PWA.
- **Étapes sensibles** (payer, signer, se connecter à un compte financier) : par défaut, l'agent prépare tout puis
  donne **un lien à ouvrir dans le navigateur de l'utilisateur** pour la dernière étape [Claude].
- Profil **jetable** par défaut ; profils persistants seulement par site et sur activation ; **aucun accès aux
  mots de passe** de l'utilisateur.
- Plus tard, en option : piloter le propre Chrome de l'utilisateur par une extension (indétectable, mais limité
  à certains onglets ou sites).
- Tout contenu lu sur le web est **non fiable** : jamais mémorisé automatiquement ; toute action qui engage
  (envoyer, payer, réserver) demande une validation.
- Version simple d'abord [Claude] : brancher un serveur MCP de navigateur existant (ex. Playwright MCP, configuré
  sur un vrai Chrome avec profil persistant) pour les outils ; l'affichage dans la web app et la reprise en main
  viennent ensuite, en natif.

### 6.8 Mémoire

- **Fichiers Markdown** par personne (`USER.md`, `MEMORY.md`, un journal daté) et pour l'espace famille, avec un
  **index plein texte SQLite**. Versionnés dans git si disponible.
- L'agent écrit par des outils `memoire.*`. Le noyau (`USER.md`, `MEMORY.md`) est injecté à chaque session et
  **plafonné en taille** : au-delà, l'agent doit fusionner ou retirer avant d'ajouter.
- **Une seule règle de sécurité** : rien de ce qui vient d'une source externe (web, e-mail, document) n'est
  mémorisé automatiquement.
- Mémoire privée jamais injectée dans l'espace famille.
- Plus tard, si besoin : consolidation périodique, recherche par le sens.

### 6.9 Rappels et proactivité

- **Minuteries** pour les rappels et **jobs récurrents** (cron, fuseau horaire). Rappels et événements stockés
  dans **CalDAV** (visibles sur les téléphones) ; **Jarvis est le seul à notifier**, sans alarme posée dans
  CalDAV, pour éviter les doublons [Claude].
- Pas de notification non urgente pendant une réunion de l'agenda.
- **Surveillance T3** (6.6), sans modèle.
- Plus tard : veille par le modèle (« y a-t-il quelque chose à signaler ? »).

### 6.10 Canaux et déploiement

- **Web app (PWA)** avec notifications push : seul canal de la famille ; seul îlot JS attendu : le service
  worker.
- **Matrix** plus tard, pour Alix. Adaptateurs fins, sans logique.
- **Voix** plus tard : un modèle vocal temps réel en façade, avec un seul outil « consulter Jarvis ».
- **Déploiement** : v1 **sur une seule machine** (autonome), accessible en web ; l'accès distant passe par les
  outils existants (Tailscale, cloudflared), pas par un tunnel maison [Claude]. Le code garde une frontière
  propre « là où s'exécute un outil » pour ajouter plus tard des **nœuds reliés** (comme les environnements de
  T3 : un cerveau, plusieurs mains).

### 6.11 Configuration

- **Un seul fichier TOML**, source de vérité. L'interface l'édite en conservant les commentaires.
- **Si le fichier est en lecture seule** (géré par l'IaC), l'interface affiche la config en lecture seule.
- **Secrets à part**, désignés par fichier ou variable d'environnement.
- **Schéma strict** : clé inconnue = refus de démarrer ; une config invalide garde la dernière valide.

### 6.12 Interface

- **Mode famille** : conversation, « ma journée » (agenda, rappels, courses), plus tard un bouton pour parler.
  Aucun vocabulaire technique. Actions visibles au moment où elles se font, validation d'un geste, affichage
  optimiste.
- **Mode admin** : threads, agents en cours (dont T3), validations en attente, navigateur, réglages.
- Leptos épinglé + `AGENTS.md` avec les usages de cette version ; `leptos-use` pour les API du navigateur ;
  tests au clavier des composants interactifs.
- **Liaison front ↔ back** : **une seule WebSocket** par onglet, messages typés partagés entre front et back
  (une crate commune, JSON ; trames binaires pour l'image du navigateur). Requêtes avec identifiant et réponse,
  plus événements poussés (texte qui s'écrit, validations, rappels). Chaque événement porte un numéro : à la
  reconnexion (téléphone en veille), le client redonne le dernier vu et le serveur renvoie la suite.
  **HTTP simple** pour le reste : fichiers de l'app, connexion, envoi et téléchargement de fichiers, et
  l'endpoint `/mcp` (imposé par la spécification MCP). App fermée : notifications Web Push.
  Pas de SSE (il faudrait de toute façon une WebSocket pour le navigateur et la voix) ; pas de server functions
  Leptos (deux façons de faire au lieu d'une). La voix pourra demander WebRTC plus tard [Claude].

### 6.13 Permissions

- La famille n'a **jamais** accès aux outils d'admin, de code, ni au navigateur sans validation.
- **Règle par défaut** [Claude, à valider] : lectures automatiques ; écritures qui ne concernent que la personne
  qui parle automatiques ; tout ce qui touche quelqu'un d'autre, sort de la maison ou engage (répondre,
  accepter une invitation, réserver, payer, valider une permission d'agent de code) demande une validation ;
  sans réponse : refus.
- Exclus par principe : gestionnaires de mots de passe, administration de machines.

## 7. Ordre de construction

Chaque étape doit être utile au quotidien avant de passer à la suivante.
1. **Hub + serveur MCP** (mémoire, agenda, rappels CalDAV, utilisables tout de suite par les agents T3 et
   Claude Code) + client MCP (MCPHub) + recherche d'outils + config TOML + un fournisseur (Claude Code via ACP)
   + web chat Leptos minimal, un seul utilisateur.
2. **Interconnexion T3** (outils `code.*` + surveillance) + Codex et Gemini + validations.
3. **PWA + notifications + famille** (plusieurs utilisateurs, espace famille, mode famille).
4. **Wasm** (`wasi-http`) + deux composants réels (ex. courses Mealie, Seerr).
5. **Navigateur** (outils d'abord, puis affichage et reprise en main dans la web app).
6. Ensuite, selon le besoin : Matrix, voix, nœuds reliés.

## 8. Laissé au constructeur

Découpage en crates, schéma SQLite, format exact du TOML, choix des crates sous les contraintes ci-dessus, briques
reprises d'autres projets (licences vérifiées).

## 9. Questions ouvertes

- **Nom** (Alix aime : Hermes, Rust, Volte — mot réel court, sens détourné, du mordant).
- **Licence** (recommandation : Apache-2.0).
- **Usages concrets de la famille** (choix des premiers composants).
- **Règle d'autonomie** (6.13) à valider.
- **Abonnements et famille** : partager un abonnement personnel sort probablement des conditions ; abonnement par
  personne ou offre familiale. À vérifier.
- **Modèle vocal** (plus tard).
