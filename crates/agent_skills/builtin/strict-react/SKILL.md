---
name: strict-react
description: 'Strict React and React Native coding standards. Use when the project uses React, React Native, or Expo, or has .tsx/.jsx files with react dependencies. Covers mandatory intermediate variables, callback naming, Jotai state layout, conditional rendering with RenderIf and RenderPlace instead of &&, single-line operator rules, consistent row heights, library priorities, animation and list libraries, and component declaration conventions. Load together with strict-typescript for the base TypeScript rules. Not for Angular projects.'
---

# Strict React / React Native

Framework-specific rules for React and React Native. The base TypeScript rules live in the `strict-typescript` skill — load both.

## ESLint

- Always adhere to the project's ESLint configuration (prettier errors may be ignored).
- Maximize type inference and narrowing; never suggest code that triggers linting errors or warnings.
- Zero `eslint-disable` / `@ts-ignore`; refactor to compliance instead.

## Variables

- Never use function or method results directly in conditionals, loops, or function arguments. Process: call → assign to a descriptive constant → use it. Applies even to simple boolean checks.

## Naming

- Use descriptive callback names instead of generic `handle` — `on[Action][Subject]` (e.g., `onSelectTagToEdit`, `onToggleReminderStatus`).

## State (Jotai)

- Organize code in `src/modules/[feature]`.
- Use atoms for state management; module state goes in `*.state.ts`.
- For complex component-local state, define atoms outside the component body to avoid re-renders.

## Conditional rendering

- Never use `&&` for conditional rendering in JSX.
- Use `<RenderPlace>` when the condition does not depend on variables used inside the component (or for type narrowing with an internal function); use `<RenderIf>` for simple boolean checks.
- Pattern: `<RenderPlace>{() => { if (item) return <Component item={item} /> }}</RenderPlace>` to avoid type casts.

## Operators

- Never use multi-line ternaries or multi-line logical operators (`??`, `||`, `&&`); extract complex logic into variables or use conditional components.

## Layout

- Keep interactive elements (selectors, inputs, icons) the same height within a row to prevent vertical layout shift (default target height is usually 40 unless specified otherwise).
- Center explicitly with flexbox (`justify-content`, `align-items`), not implicit margins.

## Libraries

- Radashi for arrays, objects, and async (`toggle`, `tryit`, `mapValues`).
- ts-essentials for advanced type magic (`DeepPartial`, `MarkOptional`).
- common-tags for string formatting (`oneLine`, `stripIndent`).
- `src/helpers` for React/UI logic (`RenderIf`, `use-incremental-render`) and typed object helpers (`atObjectKeys`). Avoid deprecated files.

## React & UI implementation

- Animations with `moti` or `reanimated`, never standard `Animated`.
- Lists always use `FlashList`.
- Group logic with comments (`// States`, `// Vars`, `// Effects`); split large components into sub-components in the same file if they are private.
- Declare components as `function ComponentName(props: Props)`; memoized: `export const ComponentName = React.memo(function ComponentName(props: Props) { ... })`.
- The primary component's props interface/type must be named exactly `Props`.
