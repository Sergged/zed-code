# Global Coding Standards — Zed Harness

Единый always-on файл: как агент работает в Zed, плюс универсальные инварианты. Правила конкретных стеков вынесены в built-in скиллы `strict-*` и подгружаются по необходимости — не дублируй их здесь.

## Скиллы стеков (strict-*)

Определи стек проекта и вызови подходящий встроенный скилл до написания или ревью кода:

| Стек                                                                             | Скилл                                  |
| -------------------------------------------------------------------------------- | -------------------------------------- |
| TypeScript / JavaScript / web (включая HTML/CSS)                                 | `strict-typescript`                    |
| Angular (`angular.json`, зависимости `@angular/*`)                               | `strict-angular` + `strict-typescript` |
| React / React Native / Expo (`.tsx`/`.jsx`, зависимости `react`, `react-native`) | `strict-react` + `strict-typescript`   |

Проекту может понадобиться несколько скиллов — загрузи каждый релевантный. Скиллы вызываются автоматически, когда задача совпадает с их описанием, либо вручную через `/` в редакторе сообщения.

Планирование — тоже скиллы: `/strict-compose-plan` (составить план в `~/.agents/plans/`, без правок кода) и `/strict-implement-plan` (реализовать утверждённый план).

## Behavior

- **Plan mode**: if the user puts you in plan mode, do not switch to code mode or start writing implementation until the user explicitly confirms.
- **No unnecessary changes**: apply global rules only to code directly touched by the task. Do not touch or refactor unrelated files or code blocks.

## Quality

- **Source over assumption**: before using any function or API from `node_modules/`, third-party packages, or external services, read its source or type definitions; do not rely on memory for parameters, return types, or side effects. Consult official docs if the source is unavailable.
- **Zero suppression**: `disable` / `ignore` comments are forbidden. Refactor instead.
- **Diagnostics**: run the diagnostics tool after changes and before finishing; fix every error and warning. Never assume the code is correct without checking.

## Tooling

- **Prefer file tools**: when a file tool fits, prefer it over the terminal for reading, searching, editing, moving, copying or deleting files — it is faster and more visible. Reach for the terminal for what no file tool covers (builds, tests, package managers, git, and so on).
- **Prefer diagnostics**: if the diagnostics tool can give you what you need (errors, warnings, type or lint information), take it from there instead of running a long compiler, linter, or build command in the terminal.
- **Mermaid**: be careful with mermaid — it often fails to render as a preview in chat (the code block is there but does not convert). Prefer a plain list or table; if you do use mermaid, keep it simple and valid.

## Subagents & images

- Zed lets the agent pick the model for a subagent: `spawn_agent` accepts an optional `model`, passed as the exact `models[].id` returned by `list_agents_and_models` for the native Zed agent (`is_native: true`). Omit it to use the configured default (`agent.subagent_model`, falling back to the parent model).
- **Images**: to inspect an image (screenshot, mockup, diagram, photo) without pulling it into the main thread context, spawn a subagent with `model: "go/deepseek-v4-flash-vision-exp"` (provider `opencode`, shown in the UI as "DeepSeek V4 Flash Vision") and ask it to describe or answer questions about the image. The image then stays inside the subagent thread — bring only the subagent's text result back into the main thread.

## Documentation integrity

Preserve existing comments and docstrings unrelated to your changes; never delete comments unless explicitly asked.
