import type { AwicModule } from "./module";

/**
 * The module registration seam. T8 and T9 each add a directory under
 * `src/modules/` and, at the bottom of its entry file, call
 * `registerModule({...})` as an import side effect — see
 * `src/modules/usb/index.ts` for the pattern, and `src/main.ts` for
 * where it gets imported. The shell (`app-shell.ts`) renders one tab per
 * registered module and lazily instantiates it via `factory()` the first
 * time its tab is selected.
 */
export interface ModuleDescriptor {
  /** Stable, unique; used as the tab's DOM identity and in the mounted-element cache. */
  readonly id: string;
  /** Human-readable tab label. */
  readonly title: string;
  /** Constructs a fresh, un-initialized module element. The shell calls `.init(ctx)` on it. */
  readonly factory: () => AwicModule;
}

const registry: ModuleDescriptor[] = [];

export function registerModule(descriptor: ModuleDescriptor): void {
  if (registry.some((m) => m.id === descriptor.id)) {
    throw new Error(`module "${descriptor.id}" is already registered`);
  }
  registry.push(descriptor);
}

export function listModules(): readonly ModuleDescriptor[] {
  return registry;
}
