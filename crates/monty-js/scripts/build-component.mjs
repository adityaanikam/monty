// Wraps the Preview 1 Rust output as a WASI 0.2 component and transpiles its
// WIT interface into JavaScript bindings for browsers and Node worker threads.

import { mkdir, readFile, rm, writeFile } from 'node:fs/promises'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { componentNew, preview1AdapterReactorPath, transpile } from '@bytecodealliance/jco'

const pkg = dirname(dirname(fileURLToPath(import.meta.url)))
const workspace = dirname(dirname(pkg))
const targetDir = process.env.CARGO_TARGET_DIR ?? join(workspace, 'target')
const corePath = join(targetDir, 'wasm32-wasip1', 'release', 'monty_wasm_runtime.wasm')
const outputDir = join(pkg, 'dist', 'worker', 'component')

const [core, adapter] = await Promise.all([readFile(corePath), readFile(preview1AdapterReactorPath())])
const component = await componentNew(core, [['wasi_snapshot_preview1', adapter]])
const { files } = await transpile(component, {
  name: 'monty.component',
  instantiation: 'async',
})
const requiredFiles = [
  'monty.component.js',
  'monty.component.core.wasm',
  'monty.component.core2.wasm',
  'monty.component.core3.wasm',
  'monty.component.core4.wasm',
]
const missingFiles = requiredFiles.filter((name) => !(name in files))
if (missingFiles.length > 0) throw new Error(`Jco output is missing: ${missingFiles.join(', ')}`)

await rm(outputDir, { recursive: true, force: true })
await Promise.all(
  Object.entries(files).map(async ([name, contents]) => {
    const destination = join(outputDir, name)
    await mkdir(dirname(destination), { recursive: true })
    await writeFile(destination, contents)
  }),
)
console.log(`built WASI 0.2 component bindings -> ${outputDir}`)
