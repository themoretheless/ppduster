/// <reference types="vite/client" />

interface PeregonPackageVersion {
  readonly name: string;
  readonly version: string;
}

declare const __PEREGON_VERSION_INFO__: {
  readonly project: PeregonPackageVersion;
  readonly engine: PeregonPackageVersion;
  readonly packages: {
    readonly npmRuntime: readonly PeregonPackageVersion[];
    readonly rustRuntime: readonly PeregonPackageVersion[];
    readonly build: readonly PeregonPackageVersion[];
  };
};

declare module "*.vue" {
  import type { DefineComponent } from "vue";
  const component: DefineComponent<Record<string, never>, Record<string, never>, unknown>;
  export default component;
}
