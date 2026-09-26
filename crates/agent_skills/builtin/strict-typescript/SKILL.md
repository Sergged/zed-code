---
name: strict-typescript
description: "Strict TypeScript and JavaScript coding standards, including general HTML/CSS rules. Use when writing or reviewing TypeScript or JavaScript in any framework, or when the project has a tsconfig.json, package.json, or .ts/.tsx/.js/.jsx files. Covers type safety and inference, forbidden destructuring and path aliasing, extraction of calls/index access/assertions out of assignments, conditionals, arguments and returns, naming and structure, operators, comments policy, and a self-audit checklist. Load together with strict-angular or strict-react for framework-specific rules."
---

# Strict TypeScript / JavaScript

Base coding standards for every TypeScript or JavaScript project (plain web, Angular, React, React Native). Load the framework-specific `strict-angular` or `strict-react` skill as well when the project uses one.

These rules mirror the `eslint-plugin-strict-code` rule set, which is the source of truth.

## Type integrity

- Full type safety; never use `any`.
- Do not annotate types TypeScript already infers (`const x: string = 'hello'`); annotate only when inference is ambiguous or wrong.
- Do not write redundant return types. Do annotate object literals; do not use inline type literals — extract a named `interface`/`type`.
- Derive types instead of duplicating them: when an annotation repeats the type of an existing variable, property or parameter, use `typeof source`. Derive types (`typeof`, `ReturnType<T>`, indexed access) instead of hand-copying shapes.
- Do not use the `public` modifier on class members — it is implicit.
- Array types use the simple form: `T[]`, not `Array<T>`.
- Do not use empty or stateless classes without a decorator.
- Stub methods must `throw new Error('Method not implemented.');`.

## Comments

- Do not add comments on your own. Write a comment only if the user explicitly asked for it in this task.
- Never delete existing comments or docstrings that are unrelated to your change.

## Destructuring & aliasing

- **No object destructuring** (`const { id } = item`, including defaults, nesting and rest). Use direct access (`item.id`). Array destructuring is allowed.
- **No path aliasing**: do not alias plain property paths (`const id = item.id;`). Use direct access (`this.s.v`) when the line is under 100 characters.
- No bridge variables (`const temp = arr[0]; this.v = temp;`).

## Extraction

- **Assignments**: a function call, index access, or type assertion must be the only top-level operation on the RHS of an assignment — never combined with operators (`!== null`, `&&`, `||`, `??`). Extract first.
- **Conditionals** (`if` / `switch` / `while` / `do-while` / `for` / ternary): no index access, type assertions, or nested ternary inside the condition. Extract first.
- **Arguments**: no binary, logical, or conditional expressions as function or constructor arguments. Extract first. _(Exception: chainable/pipe/builder receivers — see below.)_
- **Call results**: a call result must be assigned before use. Do not inline it into arrays, conditions, template literals, etc. _(Exception: chainable returns.)_
- **Returns**: do not return a logical, binary, or conditional expression directly; assign to a named variable first.
- **Function on expression**: before calling a method on something that isn't a simple variable, extract the expression first: `(currentValue ?? []).includes(inputValue)` → extract `currentValue ?? []`, then call `.includes()`. Simple property chains (`this.svc.list.value`) and single variables are allowed as receivers.
- **Chainable exception**: calls returning a monadic/chainable type (Observable `pipe`, builder, Promise-like with a callback method) and `RxJS map()` inside `.pipe()` are exempt. Assign/return chainable calls directly.

## Naming & structure

- Files: `prefix:name.ext`.
- Loop variables (`for`, `for...of`) must start with `iter` (`iterIndex`, `iterItem`). Updated state: `next[Name]`. Search keys: `key[Name]`.
- Callbacks: `on[Action][Subject]` (e.g., `onSelectTagToEdit`), not generic `handle...`.
- `interface` / `type` / `enum` names: PascalCase, and must not start with `I`/`E`/`T` followed by an uppercase letter. Enum members: StrictPascalCase.
- Variables and parameters: camelCase, never starting with `_`. `const` values may also be `UPPER_CASE` or PascalCase.
- Identifiers must be longer than one character.
- Separate logic (`*.state.ts`, `*.service.ts`) from view components.

## Operators & expressions

- No nested or unnecessary ternaries; ternaries must be single-line.
- Do not mix operators without parentheses.
- No negated conditions (`if (!x) ... else ...`).
- Use strict equality (`===` / `!==`).
- No implicit coercion; no Yoda conditions.
- Use object shorthand.
- Template expressions must resolve to a string or number.
- Enum members must be initialized.
- Avoid unnecessary conditions, prefer `readonly`, and bind methods before passing them as callbacks.

## Imports, console, formatting

- No unused imports.
- `console` only for `warn` / `error`.
- Follow the project formatter (prettier) and the `eslint-plugin-strict-code` rules; never suppress them.

## HTML / CSS / templates (general)

- No ternary operators, method calls with arguments, or arithmetic in templates; simple property binding is allowed. Extract any other logic to TS.
- Use even numbers for spacing and math.
- Use Flexbox/Grid for layout; center explicitly, not with implicit margins.

## Self-audit before finishing

1. **Destructuring**: any object destructuring? Replace with direct property access.
2. **Bridge aliasing**: did I create a variable just to assign its value to an existing property on the next line? Delete it and assign directly.
3. **Path aliasing**: did I save a plain property path (`item.id`, `this.svc.value`) into a variable? If the line is < 100 chars, use the full path inline.
4. **Extraction**: are there index accesses (`arr[0]`), type assertions (`as Type`), function calls, or binary/logical/conditional expressions directly inside an `if`/return/argument/assignment? Extract them above. Only top-level expressions count; arguments inside an already-extracted call are exempt.
5. **Comments**: did I add any comment the user did not ask for? Remove it.

## Traceability examples

```typescript
// ❌ BAD:
const b = this.f.v === Criteria.btwn; // inline
this.isB.value = b; // bridge
if (this.s.check(item.id)) { ... } // fn in if
return item.key !== a && !u; // complex return
const { value } = this.s.sel; // destructuring
const v = this.s.sel.value; // path aliasing
this.filter(v);
(c ?? []).includes(i); // fn on expression
fn(a + b); // complex argument

// ✅ GOOD:
this.isB.value = this.f.v === Criteria.btwn;
const key = item.key ?? '';
const isActive = attrs.includes(key);
const first = arr[0];
const typed = val as MyType;
const visible = key !== a && !u;
return visible;
const safe = c ?? [];
const selected = safe.includes(i);
this.data$ = this.s.getData().pipe(map(entry => this.tr(entry.id)));
```
