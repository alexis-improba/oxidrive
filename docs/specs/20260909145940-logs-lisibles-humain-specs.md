---
name: Logs lisibles pour un humain
overview: Terminal lisible, quiet par défaut ; --verbose ajoute des lignes moins importantes (même format) ; le fichier optionnel reste du JSON.
---

# Specs : Logs lisibles pour un humain

**Intention** : Je veux faire une nouvelle fonctionnalité : rendre les logs plus lisibles pour un humain.

**Plan** : [`20260909145940-logs-lisibles-humain-plan.md`](../plans/20260909145940-logs-lisibles-humain-plan.md)
**Recette** : [`20260909145940-logs-lisibles-humain-recette.md`](../recettes/20260909145940-logs-lisibles-humain-recette.md)

> Une modification des specs entraîne **obligatoirement** une révision du **plan** et de la **recette**.

## Besoins

Aujourd’hui, la console mélange horodatage machine, gravité et champs techniques devant la phrase (`paths=12 computing sync actions`), et `--verbose` bascule vers un dump trop détaillé. Le fichier de log optionnel reste du JSON (outils). L’utilisateur veut un terminal **lisible**, **quiet par défaut**, et `--verbose` qui ajoute seulement des lignes moins importantes — sans revenir à l’ancien format.

## Tâches

### Principales

#### T1 — Lire le déroulé dans le terminal

> je veux que je comprenne ce qui se passe si je lance oxidrive et je regarde le terminal

**Origine** : PO / client — « rendre les logs plus lisibles pour un humain » ; précisé ensuite : « le niveau par défaut doit être quiet », « plus verbeux avec --verbose » sans « retomber sur les anciens logs », « simplement plus de logs (moins important) »

**Dépend de** : —

**Besoin (ticket / PO / client)**

- En lançant une commande (sync, daemon, setup), chaque ligne du terminal se lit comme chez Git : une phrase en langage courant, sans horodatage ni `INFO`/`WARN`. Les avertissements commencent par `warning:`, les erreurs par `error:`. La phrase n’est pas noyée derrière des champs techniques.
- Sans flag, le terminal est **quiet** : seulement l’important (avertissements et erreurs). Une sync qui se passe bien ne raconte pas les étapes.
- `--verbose` affiche **en plus** les logs moins importants (démarrage, scan, exécution, bilan), **même** format une-ligne. Ce n’est pas un retour à l’ancien compact ni un dump debug.
- `--verbose` répété ajoute encore des lignes moins importantes, toujours dans ce format.
- `--quiet` force le même silence que le défaut (utile si la config demanderait plus).
- Un vrai terminal peut aider par la couleur ; une redirection ou un service ne doit pas polluer avec des codes couleur.

**Critères d'acceptation (ticket / PO / client)**

- [ ] Chaque événement visible tient sur une ligne scannable, forme Git : phrase seule (info), ou `warning:` / `error:` puis la phrase ; détails éventuels ensuite en `clé: valeur`. Pas d’horloge ni de `INFO`/`WARN` en capitales.
- [ ] Sans `--verbose`, une sync réussie ne montre pas les étapes ni le bilan : seulement avertissements et erreurs s’il y en a.
- [ ] Avec `--verbose`, je vois en clair le début, les étapes clés et le bilan (envoyés, reçus, ignorés, conflits), sans `paths=12 computing…` ni l’ancien format compact.
- [ ] Un avertissement et une erreur se voient **sans** `--verbose`, se distinguent des infos (`warning:` / `error:` comme Git), et tiennent en une phrase courte (chemin entre quotes s’il y en a un) — sans dump de `file_id` / `error` internes.
- [ ] `--verbose` répété ajoute davantage de lignes moins importantes, **même** format — pas un dump technique d’un autre style.
- [ ] Sans terminal interactif (redirection, service), le texte reste lisible et sans codes couleur.
- [ ] `--quiet` ne montre que avertissements et erreurs, même format que le défaut.

#### T2 — Consulter le fichier de log

> je veux que je comprenne aussi l’historique si j’ouvre le fichier de log configuré

**Origine** : PO / client — « rendre les logs plus lisibles pour un humain »

**Dépend de** : T1

**Besoin (ticket / PO / client)**

- Quand un fichier de log est configuré, le consulter fait partie de « lire les logs » : l’utilisateur veut y retrouver le déroulé, pas seulement le terminal.

**Critères d'acceptation (ticket / PO / client)**

- *(aucun issu du besoin initial seul ; voir l’arbitrage)*

**Arbitrages métier**

- 2026-09-09 — le fichier optionnel est aujourd’hui du JSON (utile aux outils, illisible à l’œil) ; le besoin de lisibilité entre en conflit avec ce format
  - Solutions : A — ne pas changer le fichier (JSON inchangé, lisibilité = terminal seulement) ; B — le fichier devient du texte humain comme le terminal ; C — les deux (texte lisible et JSON, chemins distincts)
  - Choisi : A — conserver le JSON dans le fichier de log
  - Comportement : `log_file` reste des lignes JSON (rotation journalière) ; la lisibilité humaine s’applique uniquement au terminal
  - CA : avec `log_file` configuré, chaque ligne écrite est du JSON parseable ; le format terminal (T1) n’est pas appliqué au fichier

### Finition

#### T3 — Savoir à quoi s’attendre

> je veux que la doc me dise comment se présentent les logs si je configure ou lance oxidrive

**Origine** : PO / client — « rendre les logs plus lisibles pour un humain »

**Dépend de** : T1, T2

**Besoin (ticket / PO / client)**

- La documentation d’usage décrit le format console façon Git, le défaut quiet, `--verbose` (plus de lignes moins importantes, pas l’ancien dump), `--quiet`, le niveau configuré, et le fichier JSON inchangé.

**Critères d'acceptation (ticket / PO / client)**

- [ ] Un utilisateur qui lit le README et l’exemple de configuration comprend : terminal façon Git (`warning:` / `error:`), quiet par défaut, `--verbose` = plus de logs lisibles, fichier JSON si configuré.

## Hors scope

- Journal JSONL des conflits (`.oxidrive/conflicts.log`) et la commande `status` qui le lit.
- Barres de progression du mode interactif.
- Traduction des logs ou de l’interface en français (le produit reste en anglais).
- Envoi vers journald, syslog ou un agrégateur.
- Rendre « grand public » les traces de debug des bibliothèques tierces (y compris via `--verbose` : ce n’est pas un dump de crates).
- Transformer `log_file` en texte humain (choix B/C écartés).

## Arbitrages

- 2026-09-09 — métier — T2 — format du fichier de log (JSON vs texte vs les deux) — choisi : A
- 2026-09-09 — métier — T1 — forme des lignes console (horloge + LEVEL vs Git)
  - Solutions : A — `{HH:MM:SS} {LEVEL} {message} {k=v}` ; B — forme Git (phrase nue, `warning:` / `error:`, détails `clé: valeur`, pas d’horloge)
  - Choisi : B — s’aligner sur `git` au niveau de la forme
- 2026-09-09 — métier — T1 — volume des warnings (tous les champs tracing vs phrase Git)
  - Solutions : A — afficher tous les champs `clé: valeur` ; B — phrase courte + `'chemin'` seulement (le reste dans le JSON fichier)
  - Choisi : B
  - Comportement : un `warning:` / `error:` console = une phrase façon Git ; le chemin s’il existe ; pas d’ids Drive ni de dump d’erreur interne
  - CA : une ligne d’avertissement ne contient pas `file_id:` / `error:` / `owner:` ; un doublon de nom tient en `two files named 'a'; using 'b'`
