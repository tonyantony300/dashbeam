import { useMemo } from 'react'
import { handleExternalLinkClick } from '@/lib/openExternalUrl'
import {
	type InlineNode,
	type NotesBlock,
	parseReleaseNotes,
} from '@/lib/release-notes'

// Notes are static once parsed, so position is a stable key.
function Inline({ nodes }: { nodes: InlineNode[] }) {
	return nodes.map((node, index) => {
		const key = `${node.type}-${index}`
		switch (node.type) {
			case 'bold':
				return (
					<strong key={key} className="font-medium text-foreground">
						<Inline nodes={node.children} />
					</strong>
				)
			case 'code':
				return (
					<code
						key={key}
						className="rounded bg-muted px-1 py-0.5 font-mono text-[0.85em]"
					>
						{node.text}
					</code>
				)
			case 'link':
				return (
					<a
						key={key}
						href={node.href}
						className="break-all text-foreground underline underline-offset-2"
						onClick={(event) => handleExternalLinkClick(event, node.href)}
					>
						{node.text}
					</a>
				)
			default:
				return node.text
		}
	})
}

function Block({ block }: { block: NotesBlock }) {
	switch (block.type) {
		case 'heading':
			return (
				<h3 className="mt-5 font-semibold text-foreground text-sm first:mt-0">
					<Inline nodes={block.inline} />
				</h3>
			)
		case 'list': {
			const List = block.ordered ? 'ol' : 'ul'
			return (
				<List
					className={`flex flex-col gap-1.5 ps-5 ${block.ordered ? 'list-decimal' : 'list-disc'}`}
				>
					{block.items.map((item, index) => (
						// biome-ignore lint/suspicious/noArrayIndexKey: parsed notes never reorder
						<li key={`item-${index}`}>
							<Inline nodes={item} />
						</li>
					))}
				</List>
			)
		}
		case 'rule':
			return <hr className="my-2" />
		default:
			return (
				<p>
					<Inline nodes={block.inline} />
				</p>
			)
	}
}

/** Renders the markdown subset release notes use; see `lib/release-notes`. */
export function ReleaseNotesView({ markdown }: { markdown: string }) {
	const blocks = useMemo(() => parseReleaseNotes(markdown), [markdown])
	return (
		<div className="flex flex-col gap-2.5 text-muted-foreground text-sm leading-relaxed">
			{blocks.map((block, index) => (
				// biome-ignore lint/suspicious/noArrayIndexKey: parsed notes never reorder
				<Block key={`${block.type}-${index}`} block={block} />
			))}
		</div>
	)
}
