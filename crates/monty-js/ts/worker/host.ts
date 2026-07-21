// Hosts the WIT-defined Monty component inside a Web Worker, a Node worker
// thread, or the in-process fallback. Each host owns one component instance so
// its protocol child and REPL state persist across turns.

import { WASIShim } from '@bytecodealliance/preview2-shim/instantiation'

import { instantiate } from './component/monty.component.js'

/** Core modules emitted by Jco for one transpiled WebAssembly component. */
export type ComponentModules = Readonly<Record<string, WebAssembly.Module>>

/** Status codes exposed to the transport drive loop. */
export const TurnStatus = {
  Continue: 0,
  Shutdown: 1,
} as const

/** One protobuf event envelope decoded by the Rust component. */
export interface DecodedChildEvent {
  kind: number
  bytes: Uint8Array
}

/** Result returned by a worker component dispatch. */
export interface DispatchResult {
  status: number
  events: DecodedChildEvent[]
}

/** Sends one framed request to a persistent component instance. */
export type Dispatcher = (requestFrame: Uint8Array) => Promise<DispatchResult>

/** Adapts a synchronous in-process [`WasmHost`] to the async [`Dispatcher`]. */
export function inProcessDispatcher(host: WasmHost): Dispatcher {
  return (requestFrame) => Promise.resolve(host.dispatch(requestFrame))
}

/** One instantiated Monty component, retaining its child across turns. */
export class WasmHost {
  private constructor(private readonly dispatchComponent: (request: Uint8Array) => ComponentDispatchResult) {}

  /** Instantiates all core modules and links their capability-limited WASI imports. */
  static async create(modules: ComponentModules): Promise<WasmHost> {
    const component = await instantiate((path) => getModule(modules, path), wasiImports())
    return new WasmHost(component.worker.dispatch)
  }

  /** Runs one turn and returns semantic event envelopes produced by Rust. */
  dispatch(requestFrame: Uint8Array): DispatchResult {
    const result = this.dispatchComponent(requestFrame)
    return {
      status: result.status === 'continue' ? TurnStatus.Continue : TurnStatus.Shutdown,
      events: result.events.map(({ kind, payload }) => ({ kind, bytes: payload })),
    }
  }
}

interface ComponentDispatchResult {
  status: 'continue' | 'shutdown'
  events: Array<{ kind: number; payload: Uint8Array }>
}

/** Creates isolated WASI imports with no host filesystem, environment, or network. */
function wasiImports(): Record<string, unknown> {
  const imports = new WASIShim({ sandbox: { preopens: {}, env: {}, args: [] } }).getImportObject()
  return {
    'wasi:cli/environment': imports['wasi:cli/environment'],
    'wasi:cli/exit': {
      exit: denyProcessExit,
      exitWithCode: denyProcessExit,
    },
    'wasi:cli/stderr': imports['wasi:cli/stderr'],
    'wasi:cli/stdin': imports['wasi:cli/stdin'],
    'wasi:cli/stdout': imports['wasi:cli/stdout'],
    'wasi:clocks/monotonic-clock': imports['wasi:clocks/monotonic-clock'],
    'wasi:clocks/wall-clock': imports['wasi:clocks/wall-clock'],
    'wasi:filesystem/preopens': imports['wasi:filesystem/preopens'],
    'wasi:filesystem/types': imports['wasi:filesystem/types'],
    'wasi:io/error': imports['wasi:io/error'],
    'wasi:io/streams': imports['wasi:io/streams'],
    'wasi:random/random': imports['wasi:random/random'],
  }
}

/** Turns a guest exit into a component failure instead of terminating Node. */
function denyProcessExit(): never {
  throw new Error('Monty wasm component requested process exit')
}

/** Resolves Jco's relative core-module path against the precompiled module map. */
function getModule(modules: ComponentModules, path: string): WebAssembly.Module {
  const module = modules[path] ?? modules[path.replace(/^\.\//, '')]
  if (!module) throw new Error(`component core module is missing: ${path}`)
  return module
}
