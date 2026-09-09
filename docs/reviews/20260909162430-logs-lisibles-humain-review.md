---
name: Logs lisibles pour un humain
overview: Les six tâches 🧪 du plan sont conformes aux specs — formateur Git, quiet par défaut, fichier JSON, tests et doc.
todos: []
---

# Review : Logs lisibles pour un humain

**Statut** : RAS
**Légende** : 📝 À rafiner · 🔨 À développer · 🧪 À recetter · ✅ Validé

**Specs** : [`20260909145940-logs-lisibles-humain-specs.md`](../specs/20260909145940-logs-lisibles-humain-specs.md)
**Plan** : [`20260909145940-logs-lisibles-humain-plan.md`](../plans/20260909145940-logs-lisibles-humain-plan.md)
**Recette** : [`20260909145940-logs-lisibles-humain-recette.md`](../recettes/20260909145940-logs-lisibles-humain-recette.md)

## À l'implémentation

Exécutable par `/imp-work` — contrat matrice. Lots = priorités (P0 → P2), pas des tâches PO. Uniquement les tâches `🔨 À développer`. Avancement : **ce fichier seulement**. Ne pas cocher la recette. Ne pas passer en `✅ Validé`.

## Synthèse

Les six tâches `🧪 À recetter` du plan (T1.1–T1.3, T2.1–T2.2, T3.1) collent aux CA des specs S1–S3.

`HumanFormat` (`src/logging.rs`) produit une ligne façon Git : phrase nue en info/debug, préfixe `warning:` / `error:` (ANSI jaune/rouge seulement si stderr est un TTY), champs ensuite en `clé: valeur`, sans horloge ni `INFO`/`WARN`. Le même formateur s’applique à tous les niveaux ; le compact n’est plus une branche.

Sans flag, le filtre console est `warn` (défaut `config.log_level`, `--quiet` force `warn`). `--verbose` → `oxidrive=info`, `--verbose` répété → `oxidrive=debug` (crates tiers au défaut `error` d’EnvFilter). Les étapes et le bilan du cycle (`Scanning…`, `Applying sync actions`, `Sync cycle finished` avec uploaded/downloaded/skipped/conflicts) sont en `info`. Le chemin `sync --once` n’imprime plus d’identité machine ni de bilan sur stdout. `log_file` reste `fmt::layer().json()` + rotation journalière. README, `config.example.toml`, overview, FAQ et troubleshooting décrivent quiet / Git / `--verbose` / JSON.

Barres de progression `indicatif` (hors scope) : sur un TTY, stdout peut encore montrer une barre qui se termine par « Sync complete » — ce n’est pas le canal tracing.

## Écarts specs ↔ code

| Specs | Attendu | Constaté | Gravité |
|-------|---------|----------|---------|
| `S1` / une ligne scannable | Phrase Git ; `warning:` / `error:` ; détails `clé: valeur` ; pas d’horloge ni `INFO`/`WARN` | `format_human_line` : message puis `key: value` ; préfixes Git ; pas d’horloge | — |
| `S1` / quiet sans `--verbose` | Sync réussie : pas d’étapes ni de bilan, seulement warn/error | Filtre `warn` ; étapes/bilan en `info` ; plus de `println` bilan/`Using device id` sur `handle_sync` | — |
| `S1` / `--verbose` | Début, étapes, bilan (envoyés, reçus, ignorés, conflits), pas `paths=12 computing…` | `info` engine/daemon/main ; « Computing sync actions: paths: N » (phrase avant le champ) | — |
| `S1` / warn/error sans verbose | Visibles, distincts, quoi / pourquoi, sans jargon interne | Préfixes Git ; bail anglais ; phrases cycle lisibles (conflit, trash, transfert) | — |
| `S1` / `--verbose` répété | Plus de lignes oxidrive, même format, pas un dump de crates | `oxidrive=debug` ; formateur inchangé | — |
| `S1` / hors TTY | Texte lisible, sans codes couleur | `stderr().is_terminal()` pilote ANSI du préfixe | — |
| `S1` / `--quiet` | Warn/error seulement, même format | `quiet` → `"warn"` même si config/`RUST_LOG` plus bavards | — |
| `S2` / `log_file` | JSON parseable, pas le format terminal, pas d’ANSI | `fmt::layer().json()` + `FILE_LOG_GUARD` ; filtre fichier `debug` | — |
| `S3` / doc | Quiet, Git, `--verbose` lisible, `--quiet`, fichier JSON | README flags + `config.example.toml` Logging + overview/FAQ/troubleshooting | — |

## Régressions et impacts

`print_sync_report` / `println!("Using device id: …")` absents du chemin sync. `init_logging_with_cli_flags` retiré. Unité systemd générée (`src/service.rs`) n’injecte plus `RUST_LOG` ; troubleshooting indique de réinstaller les unités anciennes. `RUST_LOG` reste l’échappatoire sans flags, comme prévu T1.1.

Call sites `tracing` du cycle (engine, executor, daemon, main, watch, store) passent par le même `HumanFormat`. `status` et `--dry-run` restent des sorties CLI `println` (hors CA quiet de la sync). Journal `.oxidrive/conflicts.log` non touché.

## Bugs, performances, sécurité

Aucun écart bloquant. Le formateur n’ajoute pas de secrets. Le layer fichier à `debug` est le contrat T2 (inchangé). Le test JSON (`file_layer_emits_parseable_json_without_ansi`) instancie un subscriber `fmt().json()` isolé plutôt que `init_logging` : proxy acceptable vu l’interdiction de ré-init du subscriber global ; le code du layer fichier est bien `.json()`.

En daemon + `--verbose`, « Sync cycle finished » est émis deux fois (`engine` puis `log_report`) — bruit, pas un CA cassé.

## Améliorations et refactorings

RAS. Quelques `warn` de chemins invalides / métadonnées illisibles restent un peu techniques (`unsafe path`, `folder-id map`) : cas dégradés, pas le déroulé d’une sync saine.

## Notes d'implémentation
