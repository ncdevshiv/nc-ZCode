/**
 * Dependency-graph rendering (SVG). Nodes come from `/tasks/{id}/dag`; the layout is a
 * layered DAG so dependency direction and critical-path weight are visible.
 */
import type { ReactNode } from "react";
import { cn } from "@/components/lib/utils.js";
import { taskTone, toneClasses } from "./format.js";
import type { TaskStatus } from "./types.js";

export interface DagNode {
  id: string;
  title: string;
  status: TaskStatus;
  role: string;
  priority: number;
  criticalPathWeight: number;
  dependsOn: string[];
  agentId: string | null;
}

const NODE_WIDTH = 148;
const NODE_HEIGHT = 46;
const H_GAP = 54;
const V_GAP = 26;

interface Placed {
  node: DagNode;
  x: number;
  y: number;
  layer: number;
}

/** Longest-path layering: a node sits after every dependency. */
export function layoutDag(nodes: DagNode[]): { placed: Placed[]; width: number; height: number } {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const layerOf = new Map<string, number>();
  const compute = (id: string, guard: Set<string>): number => {
    if (layerOf.has(id)) return layerOf.get(id) ?? 0;
    if (guard.has(id)) return 0;
    guard.add(id);
    const node = byId.get(id);
    const deps = node?.dependsOn ?? [];
    const layer = deps.length === 0 ? 0 : Math.max(...deps.map((dep) => compute(dep, guard) + 1));
    guard.delete(id);
    layerOf.set(id, layer);
    return layer;
  };
  for (const node of nodes) compute(node.id, new Set());

  const byLayer = new Map<number, DagNode[]>();
  for (const node of nodes) {
    const layer = layerOf.get(node.id) ?? 0;
    const bucket = byLayer.get(layer) ?? [];
    bucket.push(node);
    byLayer.set(layer, bucket);
  }
  const placed: Placed[] = [];
  let maxRows = 1;
  for (const [layer, bucket] of byLayer) {
    bucket.sort((a, b) => b.priority - a.priority || a.title.localeCompare(b.title));
    bucket.forEach((node, index) => {
      placed.push({
        node,
        layer,
        x: layer * (NODE_WIDTH + H_GAP) + 8,
        y: index * (NODE_HEIGHT + V_GAP) + 8,
      });
    });
    maxRows = Math.max(maxRows, bucket.length);
  }
  const width = (Math.max(...byLayer.keys(), 0) + 1) * (NODE_WIDTH + H_GAP) + 8;
  const height = maxRows * (NODE_HEIGHT + V_GAP) + 8;
  return { placed, width, height };
}

export function NcagDagView({ nodes, onSelect }: { nodes: DagNode[]; onSelect?: (id: string) => void }) {
  if (nodes.length === 0) {
    return <p className="py-8 text-center text-ui-sm text-foreground-subtle">No tasks in this plan yet.</p>;
  }
  const { placed, width, height } = layoutDag(nodes);
  const positions = new Map(placed.map((item) => [item.node.id, item]));

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      width="100%"
      height={Math.min(520, height)}
      role="img"
      aria-label="Task dependency graph"
      className="overflow-visible"
    >
      <defs>
        <marker id="ncag-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
          <path d="M 0 0 L 10 5 L 0 10 z" fill="currentColor" />
        </marker>
      </defs>
      {placed.map(({ node, x, y }) =>
        node.dependsOn.map((dep) => {
          const from = positions.get(dep);
          if (!from) return null;
          const x1 = from.x + NODE_WIDTH;
          const y1 = from.y + NODE_HEIGHT / 2;
          const x2 = x;
          const y2 = y + NODE_HEIGHT / 2;
          const mid = (x1 + x2) / 2;
          return (
            <path
              key={`${dep}->${node.id}`}
              d={`M${x1} ${y1} C${mid} ${y1} ${mid} ${y2} ${x2} ${y2}`}
              fill="none"
              stroke="currentColor"
              strokeWidth={1.4}
              markerEnd="url(#ncag-arrow)"
              className="text-border-hover"
            />
          );
        }),
      )}
      {placed.map(({ node, x, y }) => (
        <g
          key={node.id}
          transform={`translate(${x} ${y})`}
          onClick={() => onSelect?.(node.id)}
          className={cn("cursor-pointer", onSelect ? "hover:opacity-90" : undefined)}
        >
          <rect
            width={NODE_WIDTH}
            height={NODE_HEIGHT}
            rx={8}
            className={cn("fill-card stroke-current", toneClasses[taskTone(node.status)].split(" ")[0])}
            strokeWidth={1.2}
          />
          <text x={10} y={18} className="fill-foreground text-[11px] font-semibold">
            {truncate(node.title, 20)}
          </text>
          <text x={10} y={34} className="fill-current text-[10px]">
            {node.role} . {node.status}
          </text>
          {node.criticalPathWeight > 0 ? (
            <g transform={`translate(${NODE_WIDTH - 26} 8)`}>
              <circle r={10} className="fill-surface stroke-border" strokeWidth={1} />
              <text y={3} textAnchor="middle" className="fill-foreground-subtle text-[9px]">
                {node.criticalPathWeight}
              </text>
            </g>
          ) : null}
        </g>
      ))}
    </svg>
  );
}

function truncate(value: string, max: number): string {
  return value.length <= max ? value : `${value.slice(0, max - 1)}...`;
}

export function DagLegend(): ReactNode {
  return (
    <p className="mt-2 text-ui-2xs text-foreground-subtlest">
      Arrows point from a dependency to its dependent task; the badge counts how many
      tasks depend on that node (critical-path weight).
    </p>
  );
}
