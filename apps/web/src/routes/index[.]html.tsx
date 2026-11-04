import { createFileRoute, redirect } from '@tanstack/react-router'

// Both the static server and Vite expose this HTML file. Canonicalize it
// before mounting the workbench, which reads its project from the URL.
export const Route = createFileRoute('/index.html')({
  beforeLoad: ({ location }) => {
    throw redirect({
      to: '/',
      search: location.search,
      hash: location.hash,
      replace: true,
    })
  },
})
