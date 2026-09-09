---
name: Logs lisibles pour un humain
overview: Formateur une-ligne et quiet tracing sont en place, mais une sync réussie affiche encore un bilan sur stdout, et `--verbose --verbose` ouvre le debug de tous les crates.
todos:
  - id: T1.1
    content: Retirer le bilan et l’identité machine de stdout au défaut quiet
    status: accept
  - id: T2.1
    content: Limiter le filtre `--verbose` répété au crate oxidrive (pas les crates tiers)
    status: accept
  - id: T2.2
    content: Ne plus forcer RUST_LOG=info dans l’unité systemd
    status: accept
  - id: T3.1
    content: Reformuler les avertissements encore jargon (session, listing Drive)
    status: accept
---

📝 Phase Raffinage

# Review : Logs lisibles pour un humain

**Statut** : 🧪 À recetter
**Légende** : 📝 À rafiner · 🔨 À développer · 🧪 À recetter · ✅ Validé

**Specs** : [`20260909145940-logs-lisibles-humain-specs.md`](../specs/20260909145940-logs-lisibles-humain-specs.md)
**Plan** : [`20260909145940-logs-lisibles-humain-plan.md`](../plans/20260909145940-logs-lisibles-humain-plan.md)
**Recette** : [`20260909145940-logs-lisibles-humain-recette.md`](../recettes/20260909145940-logs-lisibles-humain-recette.md)

## À l'implémentation

Exécutable par `/imp-work` — contrat matrice. Lots = priorités (P0 → P2), pas des tâches PO. Uniquement les tâches `🔨 À développer`. Avancement : **ce fichier seulement**. Ne pas cocher la recette. Ne pas poser `✅ Validé`.

## Synthèse

Les six tâches `🧪 À recetter` du plan livrent le cœur T1/T2/T3 côté **tracing** : `HumanFormat` une-ligne (heure locale `%H:%M:%S`, gravité, phrase, puis champs), ANSI seulement si stderr est un TTY, défaut `warn`, `--verbose` → `info`, `--verbose` répété → `debug` (pas `trace`), `--quiet` → `warn`, formateur humain à tous les niveaux, `log_file` toujours JSON + rotation, tests du formateur et du mapping de filtre, README / `config.example.toml` / FAQ / troubleshooting / overview alignés. Le message de bail français est devenu « This file is being edited on another device ».

Écart bloquant sur le CA quiet : `oxidrive sync --once` sans flag imprime encore `Using device id` et `Sync complete: uploaded=…` sur **stdout** (`print_sync_report`), alors que le bilan tracing est déjà en `info`. La recette R1.1/R1.3 ne lit que stderr, donc elle ne verrait pas cette fuite.

`--verbose --verbose` pose `EnvFilter::new("debug")` **tous crates** : même format une-ligne, mais dump des bibliothèques (hyper, reqwest, …), contraire à T1.2 / hors-scope « pas un dump de crates ». L’unité systemd générée force `RUST_LOG=info`, ce qui contourne le défaut quiet via l’échappatoire prévu par T1.1.

## Écarts specs ↔ code

| Specs | Attendu | Constaté | Gravité |
|-------|---------|----------|---------|
| `S1` / une-ligne scannable | Heure locale, gravité, phrase, puis détails | `format_human_line` dans `src/logging.rs` : `{H:M:S} {LEVEL} {message} {k=v…}` | — |
| `S1` / quiet sans `--verbose` | Sync réussie : pas d’étapes ni de bilan, seulement warn/error | Filtre console `warn` OK ; stdout affiche encore device id + `Sync complete:…` | P0 |
| `S1` / `--verbose` | Début, étapes, bilan, même format, pas l’ancien compact | Macros cycle en `info` (engine, daemon, main) ; formateur humain unique | — |
| `S1` / warn/error sans verbose | Visibles, distincts, quoi/pourquoi | `error`/`warn` au-dessus du filtre ; quelques phrases encore jargon (session, listing) | P2 |
| `S1` / `--verbose` répété | Plus de lignes moins importantes, même format, pas un dump | Formateur inchangé ; filtre `"debug"` global = traces crates tiers | P1 |
| `S1` / pas de couleur hors TTY | Texte lisible sans ANSI | `stderr().is_terminal()` pilote `HumanFormat.ansi` et `with_ansi` | — |
| `S1` / `--quiet` | Warn/error seulement, même format | `quiet` court-circuite vers `"warn"` même si config/`RUST_LOG` plus bavards | — |
| `S2` / fichier JSON | Lignes JSON parseables, pas le format terminal | `fmt::layer().json()` + `FILE_LOG_GUARD` ; filtre fichier `debug` inchangé | — |
| `S3` / doc | Quiet, `--verbose` lisible, `--quiet`, une-ligne, JSON | README + `config.example.toml` + overview/FAQ/troubleshooting | — |

## Régressions et impacts

`print_sync_report` et `println!("Using device id: …")` dans `src/main.rs` restent le canal stdout du `--once` : call site non aligné sur le quiet tracing. Le daemon (`log_report`) est déjà en `tracing::info` — le trou est le chemin one-shot, pas la boucle.

`src/service.rs` (Linux) écrit `Environment=RUST_LOG=info` dans l’unité. T1.1 fait gagner `RUST_LOG` sur `log_level` en l’absence de flags : un `oxidrive service install` existant ou nouveau n’est **pas** quiet. launchd / Task Scheduler n’injectent pas `RUST_LOG`.

`indicatif` (`pb.finish_with_message("Sync complete")`) : hors scope (barres interactives).

## Bugs, performances, sécurité

Pas de faille nouvelle liée aux logs (pas de secret ajouté dans le formateur). Le filtre `"debug"` global n’est pas un DoS, mais il noie le terminal et peut gonfler journald si combiné à l’unité systemd.

Le test `file_layer_emits_parseable_json_without_ansi` construit un subscriber JSON isolé : il ne traverse pas `init_logging` (couche fichier vs `HumanFormat`). Comportement réel du layer fichier lu dans le code : conforme T2 ; le test est un proxy.

## Améliorations et refactorings

Plusieurs `warn!` encore en phrase technique minuscule (`skipping unsafe persisted…`, `duplicate Drive filename in folder…`) alors que T1.2 a réécrit le cycle sync. Visibles au défaut quiet **quand** ils partent (données corrompues, homonymes Drive) — polish, pas le happy path.

## T1 — P0 Critique

### T1.1 — Retirer le bilan et l’identité machine de stdout au défaut quiet

**Statut** : 🧪 À recetter
**Priorité** : P0

**Solution technique**

Supprimer `print_sync_report` et le `println!("Using device id: {device_id}")` du chemin `handle_sync` one-shot. Les équivalents tracing existent déjà (`Using this device identity`, `Sync cycle finished` dans `engine.rs` / `daemon.rs`) et ne s’affichent qu’avec `--verbose`. Ne pas toucher à `print_dry_run_summary` (sortie attendue de `--dry-run`). Après coup, une sync réussie sans flag ne doit plus écrire de bilan ni d’étapes sur stdout ni stderr.

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/main.rs` | `print_sync_report`, `println!` device id, `handle_sync` |
| `src/sync/engine.rs` | Bilan `info` « Sync cycle finished » |
| `src/daemon.rs` | `log_report` déjà en `info` |

## T2 — P1 Important

### T2.1 — Limiter le filtre `--verbose` répété au crate oxidrive (pas les crates tiers)

**Statut** : 🧪 À recetter
**Priorité** : P1

**Solution technique**

Directives `EnvFilter` ciblées : `--verbose` → `oxidrive=info` (pas `info` global) ; `--verbose` répété → `oxidrive=debug` (pas `debug` global). Les crates tiers restent au défaut d’`EnvFilter` (`error`). `RUST_LOG` inchangé comme échappatoire sans flags. Adapter les assertions de `resolve_log_filter_*` dans `src/logging.rs`.

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/logging.rs` | `resolve_log_filter_with_env`, tests de mapping |
| `src/cli.rs` | Compteur `--verbose` (inchangé) |

### T2.2 — Ne plus forcer RUST_LOG=info dans l’unité systemd

**Statut** : 🧪 À recetter
**Priorité** : P1

**Solution technique**

Retirer `Environment=RUST_LOG=info` du corps d’unité généré (ou le remplacer par `warn` si une variable d’environnement reste souhaitée). Sans flags CLI, le défaut `log_level = warn` s’applique. Documenter que pour un service bavard il faut `--verbose` dans `ExecStart` ou un `RUST_LOG` posé **volontairement**. Les unités déjà installées ne changent pas toutes seules : le correctif ne s’applique qu’au prochain `service install` (éventuellement le dire en troubleshooting).

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/service.rs` | Template systemd `Environment=RUST_LOG=info` |
| `docs/conventions/troubleshooting.md` | Conseils verbose (si mention du service) |

## T3 — P2 Polish

### T3.1 — Reformuler les avertissements encore jargon (session, listing Drive)

**Statut** : 🧪 À recetter
**Priorité** : P2

**Solution technique**

Même règle que T1.2 : phrase anglaise lisible, champs après le message, pas de « skipping unsafe… » / « duplicate Drive filename… » au défaut quiet. Cibles : `src/store/session.rs` (chargement persisté) et `src/drive/list.rs` (homonymes). Pas d’élargissement aux `debug!` déjà derrière `-vv`.

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/store/session.rs` | `warn!` chemins / payloads persistés |
| `src/drive/list.rs` | `warn!` déduplication de nom Drive |

## Notes d'implémentation

- One-shot sync : plus de `println!` device id / `print_sync_report` ; le bilan reste en `tracing::info`.
- Filtres CLI : `oxidrive=info` / `oxidrive=debug`.
- Template systemd : plus de `Environment=RUST_LOG=info` ; unités déjà installées : ré-`service install`.
