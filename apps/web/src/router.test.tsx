// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react'
import { createMemoryHistory, RouterProvider } from '@tanstack/react-router'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { getRouter } from './router'

// Exercise the generated production route tree without mounting the IDE's
// editor, runtime connections or developer tools.
vi.mock('./routes/__root', async () => {
  const { createRootRoute, Outlet } = await import('@tanstack/react-router')
  return { Route: createRootRoute({ component: Outlet }) }
})
vi.mock('./components/layout/Workbench', () => ({
  Workbench: () => <h1>IDE workbench</h1>,
}))

beforeEach(() => { vi.spyOn(window, 'scrollTo').mockImplementation(() => {}) })
afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
})

function mountAt(entry: string) {
  const history = createMemoryHistory({ initialEntries: ['/previous', entry] })
  const router = getRouter()
  router.update({ history })
  render(<RouterProvider router={router} />)
  return { history, router }
}

describe('IDE HTML entry routing', () => {
  it('opens the workbench from /index.html and replaces the alias in history', async () => {
    const { history, router } = mountAt('/index.html')
    await screen.findByRole('heading', { name: 'IDE workbench' })
    expect(router.state.location.pathname).toBe('/')
    expect(history.location.pathname).toBe('/')
    expect(history.length).toBe(2)
    history.back()
    expect(history.location.pathname).toBe('/previous')
  })

  it('preserves the selected project, other query parameters and fragment', async () => {
    const { history } = mountAt('/index.html?project=%E6%B5%8B%E8%AF%95%20line&view=monitor#variables')
    await screen.findByRole('heading', { name: 'IDE workbench' })
    const url = new URL(history.location.href, 'http://localhost')
    expect(url.pathname).toBe('/')
    expect(url.searchParams.get('project')).toBe('测试 line')
    expect(url.searchParams.get('view')).toBe('monitor')
    expect(url.hash).toBe('#variables')
    expect(history.length).toBe(2)
  })

  it('keeps the canonical root URL and its project selection unchanged', async () => {
    const { history } = mountAt('/?project=sim_smoke#variables')
    await screen.findByRole('heading', { name: 'IDE workbench' })
    expect(history.location.href).toBe('/?project=sim_smoke#variables')
    expect(history.length).toBe(2)
  })

  it.each(['/missing.html', '/nested/index.html'])('does not turn %s into the IDE entry', async (path) => {
    const { history } = mountAt(path)
    await screen.findByText('Not Found')
    expect(history.location.pathname).toBe(path)
    expect(screen.queryByRole('heading', { name: 'IDE workbench' })).toBeNull()
  })
})
