---
name: strict-angular
description: "Strict Angular coding standards. Use when the project has angular.json, @angular dependencies, or Angular components, directives and templates. Covers signals over @Input, readonly signal properties, takeUntilDestroyed on subscribe, effect/untracked placement, formControl and template binding rules (no ternaries, method calls with arguments, or arithmetic in templates), selectors, inject(), OnPush, and component API conventions. Load together with strict-typescript for the base TypeScript rules. Not for React or React Native projects."
---

# Strict Angular

Framework-specific rules for Angular. The base TypeScript rules live in the `strict-typescript` skill — load both.

These rules mirror the `eslint-plugin-strict-code` Angular rules and the `@angular-eslint` set used by the project.

## Signals & inputs

- Prefer signals over the `@Input` decorator.
- `input()` properties must NOT be `readonly`.
- `signal()`, `computed()`, `toSignal()`, `linkedSignal()`, `model()`, `contentChild()`, `viewChild()` properties must be `readonly`.
- Do not call signals without invoking them (`no-uncalled-signals`).

## Effects & subscriptions

- `effect()` and `untracked()` live only in `.component.ts`, `.directive.ts` and `.spec.ts` files, and only inside a regular class method — never in a constructor, a lifecycle hook, the class body, or a service.
- `subscribe()` inside a `@Component` / `@Directive` must go through a `.pipe()` containing `takeUntilDestroyed()` or `takeUntil()`.

## Dependency injection & lifecycle

- Prefer `inject()` over constructor parameter injection.
- Prefer `OnPush` change detection.
- Lifecycle methods must be sorted, must not be `async`, and must not be called manually.
- `computed()` callbacks must return a value.

## Component & directive API

- Component selector: element, prefix `app`, kebab-case. Class suffix `Component`.
- Directive selector: attribute, prefix `app`, camelCase.
- Outputs use `output()` emitter refs and are `readonly`.
- Prefer the `host` metadata property over `@HostBinding` / `@HostListener`; do not use `@Attribute`.
- Do not use the `queries` metadata property, `forwardRef`, or duplicate entries in metadata arrays.
- Pipes must not be impure and are prefixed with `app`.
- Use component view encapsulation, relative URL prefixes, an explicit selector, and keep inline declarations within the limit.

## Templates

- No ternary operators, method calls with arguments, or arithmetic in templates.
- Simple property binding is allowed (`{{ prop }}`, `{{ items.length }}`); extract any other logic to TypeScript.
- Static string inputs are passed without binding brackets: `title="Hello"`, not `[title]="'Hello'"`.
- Use `[formControl]`, never `formControlName`.
- Buttons must declare a `type`.
- No `any`, no non-null assertion, no interpolation inside attribute values, no empty control flow, no duplicate attributes, no nested tags.
- Prefer `@else`, `@empty`, class bindings, template literals, `ngSrc`, and a `trackBy` function.
- Keep conditional complexity low.
