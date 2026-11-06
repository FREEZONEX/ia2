// Exercise the built IDE in a real layout engine with offline snapshots.
// All API calls are mocked; the fixture never connects to a PLC or edge.
import { createServer } from 'node:http'
import { access, mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises'
import { extname, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import { parseArgs } from 'node:util'
import { webkit } from 'playwright-core'

const webRoot = fileURLToPath(new URL('..', import.meta.url))
const { values: options } = parseArgs({ options: {
  dist: { type: 'string', default: resolve(webRoot, 'dist') },
  output: { type: 'string' },
} })
const dist = resolve(options.dist)
await access(resolve(dist, 'index.html')) // Build first; never fall back to a live IDE.
const artifactRoot = resolve(webRoot, '../../target/monitor-layout')
await mkdir(artifactRoot, { recursive: true })
const output = options.output ? resolve(options.output) : await mkdtemp(resolve(artifactRoot, 'run-'))
await mkdir(output, { recursive: true })
const temporary = resolve(output, 'tmp')
await mkdir(temporary, { recursive: true })
process.env.TMPDIR = temporary
process.env.TEMP = temporary
process.env.TMP = temporary

const types = { '.js': 'text/javascript', '.css': 'text/css', '.html': 'text/html', '.svg': 'image/svg+xml', '.woff2': 'font/woff2', '.woff': 'font/woff' }
const server = createServer(async (request, response) => {
  const path = new URL(request.url, 'http://fixture').pathname
  const file = resolve(dist, path === '/' ? 'index.html' : '.' + path)
  if (!file.startsWith(dist + sep)) { response.writeHead(403).end(); return }
  try {
    const body = await readFile(file)
    response.setHeader('Content-Type', types[extname(file)] || 'application/octet-stream')
    response.end(body)
  } catch { response.writeHead(404).end() }
})
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
const origin = `http://127.0.0.1:${server.address().port}`
const measurements = []
const errors = []
const inputNames = ['actual_axis_position', 'actual_axis_position_with_long_variable_name']
let browser

try {
  browser = await webkit.launch({ headless: true, downloadsPath: resolve(temporary, 'downloads') })
  for (const width of [1836, 1280, 800, 480]) {
    let running = false
    const page = await browser.newPage({ viewport: { width, height: 1000 }, colorScheme: 'dark' })
    page.setDefaultTimeout(10_000)
    page.on('pageerror', error => errors.push(error.message))
    await page.route('**/*', async route => {
      const request = route.request()
      const url = new URL(request.url())
      if (url.origin !== origin || request.method() !== 'GET') {
        errors.push(`Unexpected request: ${request.method()} ${url.origin}${url.pathname}`)
        await route.abort()
        return
      }
      if (!url.pathname.startsWith('/api/')) { await route.continue(); return }
      const tree = { name: 'layout_fixture', path: '/offline/fixture', pous: [], pou_folders: [], devices: [], device_folders: [], edges: [], edge_folders: [], iomap: { mappings: [] }, tasks: { tasks: [], programs: [] } }
      const replies = {
        '/api/project': tree,
        '/api/projects': [{ name: tree.name, path: tree.path, is_last_opened: true }],
        '/api/iomap': tree.iomap, '/api/tasks': tree.tasks,
        '/api/runtime/status': { running, mode: running ? { kind: 'running' } : null, forces: [], device_health: [], watchdog_tripped: false, last_error: null },
        '/api/runtime/alarms': [], '/api/runtime/history': { series: [] },
        '/api/agent/status': { active: false, session: null, heartbeat: null },
        '/api/library': [], '/api/hmi': [],
      }
      if (!(url.pathname in replies)) {
        errors.push(`Unexpected API read: ${url.pathname}`)
        await route.abort()
        return
      }
      await route.fulfill({ contentType: 'application/json', body: JSON.stringify(replies[url.pathname]) })
    })
    await page.addInitScript(inputNames => {
      localStorage.setItem('ia2.theme', 'dark')
      localStorage.setItem('ia2.shell.v.v4', JSON.stringify({ editor: 35, monitor: 65 }))
      const streams = new Set()
      class FixtureEvents {
        readyState = 1
        constructor(url) { this.url = url; streams.add(this); setTimeout(() => this.onopen?.({}), 0) }
        addEventListener() {}
        removeEventListener() {}
        close() { streams.delete(this) }
      }
      window.EventSource = FixtureEvents
      const emit = event => {
        for (const stream of streams) if (stream.url.includes('/api/events')) {
          stream.onmessage?.({ data: JSON.stringify(event) })
        }
      }
      window.startLayoutRuntime = () => emit({ type: 'started' })
      window.emitLayoutSnapshot = (index, inputStale) => {
        const integers = ['0', '-9', '2147483647', '-2147483648']
        const reals = ['0', '-9', '1.2345678901234567e+100', '-1.2345678901234567e-100']
        const value = integers[index % integers.length]
        const vars = [
          { name: 'control_word', type_name: 'WORD', value: '16#0006', bits: 6 },
          { name: 'target_position', type_name: 'DINT', value, bits: 0 },
          { name: 'measured_value', type_name: 'LREAL', value: reals[index % reals.length], bits: 0 },
          { name: 'run', type_name: 'BOOL', value: index % 2 ? 'TRUE' : 'FALSE', bits: index % 2 },
          { name: 'very_long_variable_name_for_layout_stability', type_name: 'DATE_AND_TIME', value: 'DT#2026-01-01-00:00:00', bits: 0 },
          { name: 'message', type_name: 'STRING', value: index % 2 ? 'A long diagnostic message that must stay inside its value cell' : 'ready', bits: 0 },
          ...Array.from({ length: 24 }, (_, i) => ({ name: `feedback_${i}`, type_name: 'DINT', value, bits: 0 })),
        ]
        if (typeof inputStale === 'boolean') {
          vars.unshift(...inputNames.map(name => ({ name, type_name: 'DINT', value, bits: 0,
            input: { device: 'sensor_io', channel: name, stale: inputStale },
          })))
        }
        emit({ type: 'snapshot', data: {
          scan_count: index + 1, timestamp_us: (index + 1) * 100000, vars,
        } })
      }
    }, inputNames)
    await page.goto(origin + '/?project=layout_fixture')
    const monitor = page.getByRole('region', { name: 'Monitor' })
    await monitor.waitFor()
    const frames = []
    const snapshot = async (index, inputStale) => {
      await page.evaluate(([index, stale]) => window.emitLayoutSnapshot(index, stale), [index, inputStale])
      await monitor.getByText(new RegExp(`(?:^| )scan #${index + 1}$`)).waitFor()
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))))
      frames.push(await monitor.locator('table thead th').evaluateAll(nodes => nodes.map(node => {
        const rect = node.getBoundingClientRect()
        return { label: node.textContent, x: rect.x, width: rect.width }
      })))
    }
    // Read-only textual values exercise long numbers without needing an edge.
    for (let index = 0; index < 90; index++) {
      await snapshot(index)
    }
    await page.screenshot({ path: resolve(output, `monitor-${width}.png`) })
    const inputChecks = []
    for (const mode of ['read-only', 'local-running']) {
      if (mode === 'local-running') {
        running = true
        await page.evaluate(() => window.startLayoutRuntime())
      }
      for (const stale of [false, true, false]) {
        await snapshot(frames.length, stale)
        for (const name of inputNames) {
          const cell = monitor.locator(`tbody th[title="${name}"]`)
          const badge = cell.getByText(/^Stale(?: ·|$)/)
          const badgeCount = await badge.count()
          const visibility = stale && badgeCount === 1 ? await badge.evaluate(node => {
            const range = document.createRange()
            range.setStart(node.firstChild, 0)
            range.setEnd(node.firstChild, 'Stale'.length)
            const text = range.getBoundingClientRect()
            const context = document.createElement('canvas').getContext('2d')
            context.font = getComputedStyle(node).font
            const expectedWidth = context.measureText('Stale').width
            const cell = node.closest('th')
            const bounds = cell.getBoundingClientRect()
            const style = getComputedStyle(cell)
            let left = Math.max(text.left, bounds.left + parseFloat(style.paddingLeft), 0)
            let right = Math.min(text.right, bounds.right - parseFloat(style.paddingRight), innerWidth)
            let top = Math.max(text.top, 0)
            let bottom = Math.min(text.bottom, innerHeight)
            // A DOM presence/Playwright visibility check alone misses clipped text.
            for (let parent = node; parent; parent = parent.parentElement) {
              const css = getComputedStyle(parent)
              const rect = parent.getBoundingClientRect()
              if (/(hidden|clip|auto|scroll)/.test(css.overflowX)) {
                left = Math.max(left, rect.left + parent.clientLeft)
                right = Math.min(right, rect.left + parent.clientLeft + parent.clientWidth)
              }
              if (/(hidden|clip|auto|scroll)/.test(css.overflowY)) {
                top = Math.max(top, rect.top + parent.clientTop)
                bottom = Math.min(bottom, rect.top + parent.clientTop + parent.clientHeight)
              }
            }
            const visibleWidth = Math.max(0, right - left)
            const visibleHeight = Math.max(0, bottom - top)
            return { expectedWidth, visibleWidth, visibleHeight,
              passed: expectedWidth > 0 && visibleWidth >= expectedWidth - 0.5 && visibleHeight >= text.height - 0.5 }
          }) : null
          const row = cell.locator('..')
          const editors = await row.getByRole('textbox', { name: `Value for ${name}`, exact: true }).count()
          const forceButtons = await row.getByRole('button', { name: `Force ${name}`, exact: true }).count()
          const expectedControls = running && !stale ? 1 : 0
          inputChecks.push({ mode, scan: frames.length, name, stale, badgeCount, visibility, editors, forceButtons,
            passed: (stale ? badgeCount === 1 && visibility?.passed === true : badgeCount === 0)
              && editors === expectedControls && forceButtons === expectedControls })
        }
        if (stale) await page.screenshot({ path: resolve(output, `monitor-stale-${mode}-${width}.png`) })
      }
    }
    if (frames.some(frame => frame.length !== 6)) throw new Error('Expected six rendered Monitor columns')
    const deltas = frames[0].map((column, index) => ({
      label: column.label,
      xDelta: Math.max(...frames.map(frame => frame[index].x)) - Math.min(...frames.map(frame => frame[index].x)),
      widthDelta: Math.max(...frames.map(frame => frame[index].width)) - Math.min(...frames.map(frame => frame[index].width)),
    }))
    const row = monitor.getByRole('rowheader', { name: 'target_position', exact: true }).locator('..')
    if (!await row.locator('polyline').count()) throw new Error('History did not receive the changing snapshots')
    const fullValue = await monitor.getByRole('rowheader', { name: 'message', exact: true }).locator('..').locator('td').nth(3).textContent()
    if (!fullValue.includes('A long diagnostic message')) throw new Error('The long value must remain available in the DOM')
    const scrolling = await monitor.locator('table').evaluate(table => {
      const container = table.parentElement
      container.scrollLeft = container.scrollWidth
      const scrolled = container.scrollLeft
      const lastColumn = table.querySelector('thead th:last-child').getBoundingClientRect()
      const bounds = container.getBoundingClientRect()
      const lastColumnVisible = lastColumn.left >= bounds.left && lastColumn.right <= bounds.right + 1
      container.scrollLeft = 0
      return { viewport: container.clientWidth, content: container.scrollWidth, page: document.documentElement.scrollWidth, window: innerWidth, scrolled, lastColumnVisible }
    })
    if (scrolling.page > scrolling.window + 1) throw new Error('The table must not overflow the whole page')
    if (!scrolling.lastColumnVisible || (scrolling.content > scrolling.viewport + 1 && scrolling.scrolled === 0)) {
      throw new Error('All columns must remain reachable by scrolling the Monitor pane')
    }
    measurements.push({ viewport: width, frames: frames.length, deltas, scrolling, inputChecks,
      stable: deltas.every(column => column.xDelta < 0.1 && column.widthDelta < 0.1) })
    await page.close()
  }
} catch (error) {
  errors.push(error.stack || String(error))
} finally {
  await browser?.close()
  await new Promise(resolve => server.close(resolve))
}
const passed = measurements.length === 4 && measurements.every(result => result.stable && result.inputChecks.every(check => check.passed)) && errors.length === 0
const result = { browser: 'WebKit', data: 'Offline synthetic snapshots; API calls mocked, external requests blocked', measurements, errors, passed }
await writeFile(resolve(output, 'result.json'), JSON.stringify(result, null, 2) + '\n')
console.log(JSON.stringify({ passed, output, measurements, errors }, null, 2))
process.exitCode = passed ? 0 : 1
