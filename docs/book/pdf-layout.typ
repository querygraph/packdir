#set page(margin: (x: 0.7in, y: 0.75in))
#set par(leading: 0.65em)
#show raw.where(block: true): set text(size: 8pt)
#show raw.where(block: true): it => block(
  width: 100%, fill: rgb("#f1f5f6"), inset: 7pt, radius: 2pt, it)
#show figure.where(kind: table): set text(size: 8.5pt)
#show table: set text(size: 8.5pt)
#set table(inset: 4.5pt, stroke: 0.4pt + rgb("#9db4bf"))
#show figure.caption: set text(size: 8.5pt, style: "italic")
#show heading.where(level: 1): it => {
  pagebreak(weak: true)
  v(0.4em)
  text(size: 19pt, it)
  v(0.5em)
}
#show heading.where(level: 2): it => {
  v(0.8em)
  it
}
#show figure.where(kind: table): set block(breakable: true)
#show table.cell.where(y: 0): set text(weight: "bold")
#set table(fill: (_, y) => if y == 0 { rgb("#eaf1f4") })
#show table: set par(justify: false)
