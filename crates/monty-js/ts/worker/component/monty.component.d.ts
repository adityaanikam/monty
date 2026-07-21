export interface Event {
  kind: number
  payload: Uint8Array
}

export interface DispatchResult {
  status: 'continue' | 'shutdown'
  events: Event[]
}

export interface ComponentExports {
  worker: {
    dispatch(request: Uint8Array): DispatchResult
  }
}

export function instantiate(
  getCoreModule: (path: string) => WebAssembly.Module | Promise<WebAssembly.Module>,
  imports: Record<string, unknown>,
): ComponentExports | Promise<ComponentExports>
