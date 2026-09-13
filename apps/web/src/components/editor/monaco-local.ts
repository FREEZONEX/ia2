import { loader } from "@monaco-editor/react"
import * as monaco from "monaco-editor/esm/vs/editor/editor.api.js"
import "monaco-editor/esm/vs/editor/edcore.main.js"
import EditorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker"

// ST supplies its own language services. Keep the full editor features, but
// avoid loading unrelated JavaScript/JSON/CSS/HTML language packs and workers.
// Vite emits the editor worker as a local asset for desktop/offline installs.
globalThis.MonacoEnvironment = {
  getWorker: () => new EditorWorker(),
}

// Configure before React mounts Editor: the loader otherwise fetches Monaco
// from jsDelivr, which can leave an offline desktop stuck on "Loading...".
loader.config({ monaco })
