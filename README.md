# HEXFORGE Lab — serveur distant (Rust)

Serveur d'exécution pour l'outil HEXFORGE Lab : `GET /api/ping` + WebSocket
JSON-RPC (`ping` / `status` / `exec`). Compatible avec le panneau **Distant**
du Lab (bouton connect, `rpc`, exécution Java/Rust/HTML via `exec`).

## Protocole

- `GET /api/ping` → `{"server":"HEXFORGE Lab Remote API","status":"pong","timestamp":...}`
- WebSocket sur `/` (ws ou wss selon le proxy), messages JSON-RPC 2.0 :
  - `{"jsonrpc":"2.0","id":1,"method":"ping"}` → `{"status":"pong"}`
  - `{"jsonrpc":"2.0","id":2,"method":"status"}` → `{"server":...,"langs":[...]}`
  - `{"jsonrpc":"2.0","id":3,"method":"exec","params":{"lang":"java","file":"Main.java","code":"...","args":["monde"]}}`
    → `{"lang":"java","exit":0,"stdout":"...","stderr":"..."}`
    (`params.file/host/threads` acceptés mais ignorés ; `args` = tableau ou chaîne.)

Exécution réelle en sous-processus : `node` (JS, avec `ns` injecté),
`lua`/`lua5.4`/`luajit` (table `ns` injectée), `javac`+`java` (fichier `Main.java`),
`rustc`+binaire (`main.rs`). HTML = analyse (`has_form`, `has_script`, taille —
le rendu reste côté client). Erreurs de compilation = `exit != 0` + `stderr`,
pas d'erreur JSON-RPC (sauf timeout, code trop gros, langage inconnu).

Limites : 200 Ko de code, 25 s par exécution, 2 exécutions simultanées,
sorties tronquées à 4000 caractères.

## Lancer en local

```bash
cargo run
# écoute sur :3000 (ou $PORT). Tester :
curl localhost:3000/api/ping
```

Runtimes optionnels selon les langages voulus : `node`, `lua`, `javac`/`java`, `rustc`.

## Docker

```bash
docker build -t hexforge-server .
docker run -p 10000:10000 -e PORT=10000 hexforge-server
```

## Déployer sur Render — réglages exacts (runtime natif Rust)

Ne mélangez pas les deux champs (erreur vue en prod : la commande de build
lancée comme Start Command rebuild à chaque démarrage puis quitte → aucun
port ouvert, « Application exited early ») :

- **Build Command :** `cargo build --release`
- **Start Command :** `./target/release/hexforge-server`
  (le nom = `[package] name` du Cargo.toml, vérifiable par
  `cargo build --release && ls target/release/` **dans le Build Command**,
  pas dans le Start).
- **Health Check Path :** `/api/ping` (évite « No open ports detected »).
- `$PORT` est injecté par Render et déjà lu par le serveur.

Note : l'image Rust native contient rustc/cargo (exec Rust OK) mais **ni
`node`, ni `lua`, ni `java`** → `exec` JS/Lua/Java répondra « runtime
manquant ». Pour le multi-langage complet, passez en **Runtime: Docker**
(Settings → Runtime → Docker, Dockerfile Path `./Dockerfile`, Start Command
vide) : l'image installe déjà nodejs + lua5.4 + jdk + rustc.

## Déployer sur Render (Docker, recommandé)

1. Poussez ce dossier comme dépôt git (ou repo avec ce dossier à la racine).
2. Render → New → Web Service → **Docker** (pas de runtime Rust natif).
3. Render injecte `$PORT` tout seul (le serveur le lit déjà).
4. URL obtenue : `https://<nom>.onrender.com` → dans le Lab :
   `rconnect <nom>.onrender.com 443 wss` + bouton `tester /api/ping`.
   (Premier appel lent : l'instance gratuite dort après inactivité.)

## Sécurité

Pas de sandbox : le code reçu tourne en clair sur la machine (fichiers
temporaires supprimés après chaque run, timeout + kill garantis, 2 runs max
en parallèle). N'exposez ce serveur publiquement que pour du code de
confiance, ou durcissez (TODO possibles : gVisor/Firecracker, auth par token
dans `exec.params`, allowlist d'IPs, quotas par IP).
