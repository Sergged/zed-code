# Global Coding Standards — Zed Harness

Единый always-on файл: как агент работает в Zed, плюс универсальные инварианты. Правила конкретных стеков вынесены в built-in скиллы `strict-*` и подгружаются по необходимости (мандат на проактивную загрузку живёт в системном промпте — не дублируй его здесь).

## Behavior

- **Plan mode**: if the user puts you in plan mode, do not switch to code mode or start writing implementation until the user explicitly confirms.
- **No unnecessary changes**: apply global rules only to code directly touched by the task. Do not touch or refactor unrelated files or code blocks.

## Quality

- **Source over assumption**: before using any function or API from `node_modules/`, third-party packages, or external services, read its source or type definitions; do not rely on memory for parameters, return types, or side effects. Consult official docs if the source is unavailable.
- **Zero suppression**: `disable` / `ignore` comments are forbidden. Refactor instead.

## Documentation integrity

Preserve existing comments and docstrings unrelated to your changes; never delete comments unless explicitly asked.
