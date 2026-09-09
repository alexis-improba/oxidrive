---
name: Logs lisibles pour un humain
overview: Formateur console humain, défaut quiet, --verbose = plus de lignes info (pas l’ancien dump), fichier JSON conservé, tests et doc.
todos:
  - id: T1.1
    content: Extraire un formateur console façon Git (warning:/error:, phrase, détails)
    status: accept
  - id: T1.2
    content: Réécrire les messages et les classer (warn/error au défaut, info au verbose)
    status: accept
  - id: T1.3
    content: Rédiger les tests unitaires du formateur et du filtrage quiet/verbose
    status: accept
  - id: T2.1
    content: Conserver le layer JSON de log_file (rotation inchangée)
    status: accept
  - id: T2.2
    content: Rédiger les tests de non-régression JSON du fichier
    status: accept
  - id: T3.1
    content: Mettre à jour README et config.example.toml
    status: accept
---

# Plan : Logs lisibles pour un humain

**Statut** : 🧪 À recetter
**Légende** : 📝 À rafiner · 🔨 Approuvé · 🧪 À recetter · ✅ Recetté

**Specs** : [`20260909145940-logs-lisibles-humain-specs.md`](../specs/20260909145940-logs-lisibles-humain-specs.md)
**Recette** : [`20260909145940-logs-lisibles-humain-recette.md`](../recettes/20260909145940-logs-lisibles-humain-recette.md)

## Arbitrages

- 2026-09-09 — métier — T2 — format du fichier de log — choisi : A
- 2026-09-09 — métier — T1 — forme console inspirée de Git — choisi : B (`warning:` / `error:`, pas d’horloge)
- 2026-09-09 — métier — T1 — volume des warnings — choisi : B (phrase + `'chemin'`, pas le dump de champs)
- 2026-09-09 — technique — T1.2 — réécriture in-place vs couche d’événements métier — choisi : A

## À l'implémentation

Exécutable par `/imp-work` — contrat matrice. Uniquement les tâches `🔨 Approuvé`. Ne pas redéfinir. Ne pas cocher la recette. Ne pas poser `✅ Recetté`.

> Une modification de ce plan entraîne **obligatoirement** une révision de la recette.

## T1 — Lire le déroulé dans le terminal

**Objectif** : je veux que je comprenne ce qui se passe si je lance oxidrive et je regarde le terminal

### T1.1 — Extraire un formateur console façon Git (warning:/error:, phrase, détails)

**Statut** : 🧪 À recetter

**Solution technique**

Remplacer le `fmt::layer().compact()` de `src/logging.rs` par un `FormatEvent` (ou équivalent `tracing-subscriber`) qui produit **une ligne** façon Git : phrase seule pour l’info (détails `clé: valeur` possibles) ; `warning:` / `error:` comme `git`, phrase courte, `'chemin'` si présent, **sans** dump des autres champs tracing (ils restent dans le JSON fichier). Pas d’horloge, pas de `INFO`/`WARN` en capitales. ANSI seulement si stderr est un TTY (`IsTerminal`) : colorer le préfixe (`warning` jaune, `error` rouge). Extraire la mise en forme dans une fonction testable, sans ré-init du subscriber global.

Changer `resolve_log_filter` : sans flag → `warn` (quiet), même si `config.log_level` vaut encore `info` / `debug` (anciens fichiers) ; `--verbose` → `oxidrive=info` ; `--verbose` répété → `oxidrive=debug` (crates tiers au défaut `error`) ; `--quiet` → `warn`. `RUST_LOG` reste un échappatoire sans flags. Layer fichier JSON inchangé (T2, filtre `debug` actuel).

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/logging.rs` | Subscriber console compact + JSON fichier |
| `src/main.rs` | `resolve_log_filter`, `init_tracing` |
| `src/config.rs` | Défaut `log_level` |
| `Cargo.toml` | `tracing-subscriber` (`ansi`, `json`, `env-filter`), `chrono` |

### T1.2 — Réécrire les messages et les classer (warn/error au défaut, info au verbose)

**Statut** : 🧪 À recetter

**Dépend de** : T1.1

**Solution technique**

Réécrire in-place les macros du cycle visible, phrases anglaises **façon Git** (minuscule après le préfixe, un fait par ligne). **Classer par importance** : `error` / `warn` = visible au défaut quiet ; `info` = étapes et bilan (uniquement avec `--verbose`) ; `debug` / `trace` = `--verbose` répété, pas les crates tiers. Les champs tracing restent pour le JSON ; le formateur console n’en montre que le chemin. Aligner le message français isolé (`fichier {} édité par {}`) sur l’anglais. Ne pas « rétrograder » le terminal vers le compact en montant la verbosité.

**Arbitrages technique**

- 2026-09-09 — même résultat utilisateur, deux façons de porter les phrases
  - Solutions :
    - A — réécrire les macros `tracing` sur place : court, peu de fichiers nouveaux · moyen, format et contenu restent couplés · long, dérive facile mais surface minimale
    - B — couche d’événements métier (helpers / macros dédiés) : court, plus de code · moyen, séparation nette formateur / métier · long, discipline à tenir, plus lourd que le besoin
  - Privilégiée : A — le crate utilise déjà `tracing` partout ; une couche parallèle n’apporte pas de résultat utilisateur différent
  - Choisi : A

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/sync/engine.rs` | Étapes et bilan du cycle |
| `src/sync/executor.rs` | Transferts, suppressions différées, baux |
| `src/daemon.rs` | Boucle, signaux, échecs de cycle |
| `src/main.rs` | Setup, sync, dry-run |
| `src/watch/local.rs` | Avertissements watcher |
| `src/store/session.rs` | Persist / chemins rejetés |
| `docs/conventions/code-style.md` | Niveaux `tracing` (inchangés) |

### T1.3 — Rédiger les tests unitaires du formateur et du filtrage quiet/verbose

**Statut** : 🧪 À recetter

**Dépend de** : T1.1

**Solution technique**

Tests dans `src/logging.rs` (`#[cfg(test)]`) : formater un événement factice (préfixe Git, phrase, champs `clé: valeur`, une ligne, pas d’ANSI si demandé). Tester `resolve_log_filter` (l’extraire vers `logging` si besoin) : quiet → `warn` ; verbose 0 → `warn` (défaut) ; verbose 1 → `info` ; verbose 2 → `debug`. Pas de ré-init du subscriber global. Le formateur est le même dans tous les cas (pas de branche « ancien compact »).

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/logging.rs` | Formateur à tester |
| `src/main.rs` | `resolve_log_filter` |

## T2 — Consulter le fichier de log

**Objectif** : je veux que je comprenne aussi l’historique si j’ouvre le fichier de log configuré

### T2.1 — Conserver le layer JSON de log_file (rotation inchangée)

**Statut** : 🧪 À recetter

**Dépend de** : T1.1

**Solution technique**

Laisser `fmt::layer().json()` + `tracing-appender` (rotation journalière, création du répertoire parent, `FILE_LOG_GUARD`). Le formateur humain de T1.1 s’applique **uniquement** à la couche console. Ne pas réutiliser `FormatEvent` sur le writer fichier. `config.log_file` inchangé.

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/logging.rs` | Layer fichier JSON + garde `FILE_LOG_GUARD` |
| `src/config.rs` | `log_file` |
| `config.example.toml` | Commentaire JSON lines |

### T2.2 — Rédiger les tests de non-régression JSON du fichier

**Statut** : 🧪 À recetter

**Dépend de** : T2.1

**Solution technique**

Test de non-régression : une ligne de la couche fichier est du JSON parseable (champs tracing habituels), pas le format une-ligne console, pas d’ANSI. Writer mémoire ou `tempfile`. Ne pas dépendre du subscriber global déjà initialisé. Ne pas confondre avec `.oxidrive/conflicts.log`.

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `src/logging.rs` | Layer fichier |
| `src/sync/observability.rs` | JSONL conflits — **ne pas** confondre avec `log_file` |

## T3 — Savoir à quoi s’attendre

**Objectif** : je veux que la doc me dise comment se présentent les logs si je configure ou lance oxidrive

### T3.1 — Mettre à jour README et config.example.toml

**Statut** : 🧪 À recetter

**Dépend de** : T1.1, T2.1

**Solution technique**

README (flags globaux) et `config.example.toml` : défaut quiet (`log_level = "warn"`), format console façon Git, `--verbose` = plus de lignes lisibles (pas l’ancien dump), `--quiet`, fichier JSON inchangé. Aligner troubleshooting / FAQ / overview si `--verbose --verbose` y est encore décrit comme un dump tracing.

**Code existant concerné**

| Chemin | Rôle |
|--------|------|
| `README.md` | `--verbose` / `--quiet` |
| `config.example.toml` | `log_level`, `log_file` |
| `docs/conventions/troubleshooting.md` | Conseils verbose |
| `docs/conventions/faq.md` | Idem |
| `docs/architecture/overview.md` | Ligne « Logging » du tableau des choix |

## Notes d'implémentation

- `--verbose` / `--verbose` répété filtrent `oxidrive=info` / `oxidrive=debug` (pas un `debug` global).
- L’unité systemd générée n’injecte plus `RUST_LOG=info` ; réinstaller le service pour les unités déjà posées.
- `init_logging_with_cli_flags` (bool verbose → debug) a été retiré : plus d’appelants, et il contredisait le mapping `-v` = `info`.
- Pas de `CHANGELOG.md` à la racine du dépôt.
