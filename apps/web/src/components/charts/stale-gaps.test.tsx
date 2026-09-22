// @vitest-environment jsdom
import { cleanup, render } from "@testing-library/react"
import { afterEach, expect, it } from "vitest"
import { Sparkline } from "./Sparkline"
import { TrendChart } from "./TrendChart"

afterEach(cleanup)

it("does not connect sparkline segments across stale samples", () => {
  const { container } = render(<Sparkline values={[1, 2, null, 3, 4]} filled />)
  expect(container.querySelectorAll("polyline")).toHaveLength(2)
  expect(container.querySelectorAll("path")).toHaveLength(2)
  expect(container.innerHTML).not.toContain("NaN")
})

it("gaps trend lines and bands, and reports an unknown latest sample", () => {
  const points = [1, 2, null, 3, 4, null].map((v, t) => ({ t, v, lo: v ?? 1, hi: v === null ? 10 : v + 1 }))
  const { container } = render(<TrendChart series={[{ name: "level", points, binary: false, color: "red" }]} />)
  expect(container.querySelectorAll("polyline")).toHaveLength(2)
  expect(container.querySelector("path")?.getAttribute("d")?.match(/M/g)).toHaveLength(2)
  expect(container.textContent).toContain("level—")
  expect(container.innerHTML).not.toContain("NaN")
})
