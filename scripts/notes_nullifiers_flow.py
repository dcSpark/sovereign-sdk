#!/usr/bin/env python3
"""
Render an ASCII UTXO-style flow diagram from a JSON dump of `notes_nullifiers`.

Input format: a JSON array of rows like:
[
  {
    "cm": "hex(32b)",
    "cm_ins": "[\"hex(32b)\", ...]"  # may be JSON string or actual array or null
    "created_tx_hash": "0x...",
    "created_kind": "deposit|transfer|withdraw",
    "value": "100",
    "recipient": "privpool1...",
    "sender_id": "privpool1...",
    ...
  },
  ...
]

Examples:
  python3 scripts/notes_nullifiers_flow.py --input notes.json
  python3 scripts/notes_nullifiers_flow.py notes.json
  cat notes.json | python3 scripts/notes_nullifiers_flow.py --paths --values
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Dict, List, Optional, Sequence, Set, Tuple


def _strip_0x(s: str) -> str:
    s = s.strip()
    return s[2:] if s.lower().startswith("0x") else s


def _is_zero_hash32(s: str) -> bool:
    s = _strip_0x(s).lower()
    return s == ("0" * 64)


def _short_hex(s: str, left: int = 6, right: int = 6) -> str:
    """Shorten a hex string, keeping 0x prefix."""
    s = _strip_0x(s)
    if len(s) <= left + right + 2:
        return f"0x{s}"
    return f"0x{s[:left]}…{s[-right:]}"


def _short_addr(s: str, left: int = 6, right: int = 6) -> str:
    """
    Shorten an address like privpool1zj7w3fxv0qvp4sqn6tgqk5503yz28xmcmqznfccuu3t6pww84hksa679fp
    to: privpool1zj7w3...a679fp (keeping prefix + start chars + end chars)
    """
    if not s:
        return ""
    
    # Find the prefix (everything up to and including the '1' for bech32-style addresses)
    # or just use the whole thing if it's short enough
    prefix_end = s.find('1')
    if prefix_end != -1:
        prefix = s[:prefix_end + 1]  # e.g., "privpool1"
        rest = s[prefix_end + 1:]    # e.g., "zj7w3fxv0qvp4sqn6tgqk5503yz28xmcmqznfccuu3t6pww84hksa679fp"
    else:
        # No '1' found, treat as hex
        prefix = "0x"
        rest = _strip_0x(s)
    
    # If short enough, return as-is
    if len(rest) <= left + right + 3:
        return s
    
    # Format: prefix + first N chars + ... + last M chars
    return f"{prefix}{rest[:left]}...{rest[-right:]}"


def _parse_iso8601(dt: Optional[str]) -> Optional[datetime]:
    if not dt:
        return None
    try:
        return datetime.fromisoformat(dt)
    except Exception:
        return None


def _parse_cm_ins(value: Any) -> List[str]:
    if value is None:
        return []
    if isinstance(value, list):
        out: List[str] = []
        for item in value:
            if isinstance(item, str) and item.strip():
                out.append(_strip_0x(item).lower())
        return out
    if isinstance(value, str):
        s = value.strip()
        if not s:
            return []
        try:
            parsed = json.loads(s)
        except Exception:
            return [_strip_0x(s).lower()]
        return _parse_cm_ins(parsed)
    return []


@dataclass
class Note:
    cm: str
    value: Optional[str] = None
    recipient: Optional[str] = None
    sender_id: Optional[str] = None
    cm_ins: List[str] = field(default_factory=list)
    created_tx_hash: Optional[str] = None
    created_at: Optional[datetime] = None
    created_kind: Optional[str] = None
    spent_tx_hash: Optional[str] = None
    spent: bool = False


@dataclass
class TxGroup:
    tx_hash: str
    kind: Optional[str] = None
    created_at: Optional[datetime] = None
    inputs: Set[str] = field(default_factory=set)
    outputs: List[Note] = field(default_factory=list)


def _read_rows(path: str) -> List[Dict[str, Any]]:
    if path == "-" or path == "":
        data = sys.stdin.read()
    else:
        with open(path, "r", encoding="utf-8") as f:
            data = f.read()
    parsed = json.loads(data)
    if not isinstance(parsed, list):
        raise ValueError("expected a JSON array")
    rows: List[Dict[str, Any]] = []
    for item in parsed:
        if isinstance(item, dict):
            rows.append(item)
    return rows


def _build_notes(rows: Sequence[Dict[str, Any]]) -> Dict[str, Note]:
    notes: Dict[str, Note] = {}
    for row in rows:
        cm = str(row.get("cm", "")).strip()
        if not cm:
            continue
        cm_norm = _strip_0x(cm).lower()
        note = Note(
            cm=cm_norm,
            value=row.get("value"),
            recipient=row.get("recipient"),
            sender_id=row.get("sender_id"),
            cm_ins=[c for c in _parse_cm_ins(row.get("cm_ins")) if not _is_zero_hash32(c)],
            created_tx_hash=row.get("created_tx_hash"),
            created_at=_parse_iso8601(row.get("created_at")),
            created_kind=row.get("created_kind"),
            spent_tx_hash=row.get("spent_tx_hash"),
            spent=row.get("spent_tx_hash") is not None,
        )
        notes[cm_norm] = note
    return notes


def _build_tx_groups(notes: Dict[str, Note]) -> List[TxGroup]:
    by_tx: Dict[str, TxGroup] = {}
    for note in notes.values():
        tx = note.created_tx_hash
        if not tx:
            continue
        g = by_tx.get(tx)
        if g is None:
            g = TxGroup(tx_hash=tx)
            by_tx[tx] = g
        if g.kind is None:
            g.kind = note.created_kind
        if g.created_at is None:
            g.created_at = note.created_at
        g.outputs.append(note)
        for cm_in in note.cm_ins:
            g.inputs.add(cm_in)

    def key(g: TxGroup) -> Tuple[int, str]:
        dt = g.created_at
        return (0 if dt is not None else 1, dt.isoformat() if dt else g.tx_hash)

    return sorted(by_tx.values(), key=key)


def _build_note_graph(notes: Dict[str, Note]) -> Tuple[Dict[str, Set[str]], Dict[str, int]]:
    adj: Dict[str, Set[str]] = {}
    indeg: Dict[str, int] = {}

    def add_node(n: str) -> None:
        adj.setdefault(n, set())
        indeg.setdefault(n, 0)

    for note in notes.values():
        add_node(note.cm)
        for cm_in in note.cm_ins:
            add_node(cm_in)
            if note.cm not in adj[cm_in]:
                adj[cm_in].add(note.cm)
                indeg[note.cm] += 1

    return adj, indeg


# ═══════════════════════════════════════════════════════════════════════════════
# ASCII BOX RENDERING
# ═══════════════════════════════════════════════════════════════════════════════

def _render_utxo_box(
    cm: str,
    value: Optional[str] = None,
    recipient: Optional[str] = None,
    sender: Optional[str] = None,
    spent: bool = False,
    show_values: bool = True,
    show_addresses: bool = True,
) -> List[str]:
    """Render a single UTXO as an ASCII box."""
    lines: List[str] = []
    cm_short = _short_hex(cm)
    
    # Determine box style based on spent status
    if spent:
        h_line = "─"
        corner_tl, corner_tr, corner_bl, corner_br = "┌", "┐", "└", "┘"
        v_line = "│"
        status = "SPENT"
    else:
        h_line = "═"
        corner_tl, corner_tr, corner_bl, corner_br = "╔", "╗", "╚", "╝"
        v_line = "║"
        status = "UNSPENT"
    
    # Build content lines
    content: List[str] = []
    content.append(f"📦 {cm_short}")
    
    if show_values and value:
        content.append(f"   💰 {value}")
    
    if show_addresses:
        if recipient:
            content.append(f"   → {_short_addr(recipient, 6, 6)}")
        if sender:
            content.append(f"   ← {_short_addr(sender, 6, 6)}")
    
    content.append(f"   [{status}]")
    
    # Calculate box width
    max_len = max(len(line) for line in content)
    box_width = max_len + 4
    
    # Render box
    lines.append(f"{corner_tl}{h_line * box_width}{corner_tr}")
    for line in content:
        padding = box_width - len(line)
        lines.append(f"{v_line} {line}{' ' * (padding - 1)}{v_line}")
    lines.append(f"{corner_bl}{h_line * box_width}{corner_br}")
    
    return lines


def _render_tx_box(tx_hash: str, kind: str) -> List[str]:
    """Render a transaction as a small box."""
    tx_short = _short_hex(tx_hash, 4, 4)
    label = f"TX:{kind.upper()}"
    content = f"⚡ {label}"
    width = max(len(content), len(tx_short)) + 4
    
    return [
        f"┌{'─' * width}┐",
        f"│ {content}{' ' * (width - len(content) - 1)}│",
        f"│ {tx_short}{' ' * (width - len(tx_short) - 1)}│",
        f"└{'─' * width}┘",
    ]


def _center_text(text: str, width: int) -> str:
    if len(text) >= width:
        return text
    padding = (width - len(text)) // 2
    return " " * padding + text


def _render_flow_diagram(
    notes: Dict[str, Note],
    groups: List[TxGroup],
    *,
    show_values: bool = True,
    show_addresses: bool = True,
) -> str:
    """Render the full UTXO flow as an ASCII diagram."""
    lines: List[str] = []
    
    lines.append("")
    lines.append("╔════════════════════════════════════════════════════════════════════════════════╗")
    lines.append("║                           UTXO FLOW DIAGRAM                                    ║")
    lines.append("╚════════════════════════════════════════════════════════════════════════════════╝")
    lines.append("")
    
    for i, group in enumerate(groups):
        tx_hash = group.tx_hash
        kind = group.kind or "unknown"
        inputs = sorted(group.inputs)
        outputs = group.outputs
        
        # Section header
        lines.append(f"{'─' * 80}")
        lines.append(f"  Transaction #{i+1}: {kind.upper()}")
        lines.append(f"  Hash: {_short_hex(tx_hash, 8, 8)}")
        if group.created_at:
            lines.append(f"  Time: {group.created_at.strftime('%Y-%m-%d %H:%M:%S')}")
        lines.append(f"{'─' * 80}")
        lines.append("")
        
        # Render inputs
        if inputs:
            lines.append("  INPUTS (consumed UTXOs):")
            lines.append("  " + "─" * 40)
            for cm_in in inputs:
                in_note = notes.get(cm_in)
                if in_note:
                    box = _render_utxo_box(
                        cm_in,
                        value=in_note.value,
                        recipient=in_note.recipient,
                        sender=in_note.sender_id,
                        spent=True,
                        show_values=show_values,
                        show_addresses=show_addresses,
                    )
                else:
                    box = _render_utxo_box(cm_in, spent=True, show_values=False, show_addresses=False)
                for line in box:
                    lines.append(f"    {line}")
                lines.append("")
        else:
            lines.append("  INPUTS: ∅ (MINT/DEPOSIT)")
            lines.append("")
        
        # Arrow
        lines.append("          │")
        lines.append("          │")
        lines.append("          ▼")
        lines.append("")
        
        # Transaction box
        tx_box = _render_tx_box(tx_hash, kind)
        for line in tx_box:
            lines.append(f"    {line}")
        lines.append("")
        
        # Arrow
        lines.append("          │")
        lines.append("          │")
        lines.append("          ▼")
        lines.append("")
        
        # Render outputs
        lines.append("  OUTPUTS (created UTXOs):")
        lines.append("  " + "─" * 40)
        for out_note in outputs:
            box = _render_utxo_box(
                out_note.cm,
                value=out_note.value,
                recipient=out_note.recipient,
                sender=out_note.sender_id,
                spent=out_note.spent,
                show_values=show_values,
                show_addresses=show_addresses,
            )
            for line in box:
                lines.append(f"    {line}")
            lines.append("")
        
        lines.append("")
    
    return "\n".join(lines)


def _render_chain_diagram(
    notes: Dict[str, Note],
    *,
    show_values: bool = True,
    show_addresses: bool = True,
    max_chains: int = 20,
) -> str:
    """Render UTXO chains as connected ASCII diagrams."""
    adj, indeg = _build_note_graph(notes)
    
    # Find roots (UTXOs with no inputs, i.e., sources)
    roots = sorted([n for n, d in indeg.items() if d == 0])
    if not roots:
        roots = sorted(adj.keys())
    
    lines: List[str] = []
    lines.append("")
    lines.append("╔════════════════════════════════════════════════════════════════════════════════╗")
    lines.append("║                           UTXO CHAIN PATHS                                     ║")
    lines.append("╚════════════════════════════════════════════════════════════════════════════════╝")
    lines.append("")
    
    chains_printed = 0
    
    def render_chain(path: List[str], chain_num: int) -> None:
        nonlocal chains_printed
        
        # Chain header
        lines.append(f"╔═══ Chain #{chain_num} " + "═" * 55 + "╗")
        lines.append("║")
        
        for i, cm in enumerate(path):
            note = notes.get(cm)
            is_last = i == len(path) - 1
            
            # Get note info
            value = note.value if note else None
            recipient = note.recipient if note else None
            sender = note.sender_id if note else None
            spent = note.spent if note else False
            
            cm_short = _short_hex(cm)
            
            # Build status indicator and info
            if not spent:
                status_icon = "🟢"
                status_text = "LIVE"
            else:
                status_icon = "🔴"
                status_text = "SPENT"
            
            # Build the UTXO box
            if not spent:
                # Live UTXO - double line box
                top_line    = "╔" + "═" * 62 + "╗"
                bottom_line = "╚" + "═" * 62 + "╝"
                side = "║"
            else:
                # Spent UTXO - single line box
                top_line    = "┌" + "─" * 62 + "┐"
                bottom_line = "└" + "─" * 62 + "┘"
                side = "│"
            
            # Format content lines
            cm_line = f"{cm_short}  {status_icon} {status_text}"
            if show_values and value:
                cm_line += f"  💰 {value}"
            
            if show_addresses:
                addr_parts = []
                if sender:
                    addr_parts.append(f"← {_short_addr(sender, 6, 6)}")
                if recipient:
                    addr_parts.append(f"→ {_short_addr(recipient, 6, 6)}")
                addr_line = "   ".join(addr_parts) if addr_parts else ""
            else:
                addr_line = ""
            
            # Render UTXO box
            lines.append(f"║     {top_line}")
            lines.append(f"║     {side}  {cm_line:<60}{side}")
            lines.append(f"║     {side}{' ' * 62}{side}")
            if addr_line:
                lines.append(f"║     {side}  {addr_line:<60}{side}")
            lines.append(f"║     {bottom_line}")
            
            # Arrow to next UTXO (if not last)
            if not is_last:
                lines.append("║                                  │")
                lines.append("║                                  ▼")
        
        # Chain footer
        lines.append("║")
        lines.append("╚" + "═" * 68 + "╝")
        lines.append("")
        lines.append("")
    
    def dfs(cur: str, path: List[str], seen: Set[str]) -> None:
        nonlocal chains_printed
        if chains_printed >= max_chains:
            return
        if cur in seen:
            return
        seen.add(cur)
        path.append(cur)
        
        nexts = sorted(adj.get(cur, set()))
        if not nexts:
            # End of chain - render it
            chains_printed += 1
            render_chain(path, chains_printed)
        else:
            for nxt in nexts:
                if chains_printed >= max_chains:
                    break
                dfs(nxt, path, seen)
        
        path.pop()
        seen.remove(cur)
    
    for r in roots:
        if chains_printed >= max_chains:
            break
        dfs(r, [], set())
    
    if chains_printed >= max_chains:
        lines.append(f"... (truncated after {max_chains} chains)")
    
    return "\n".join(lines)


def _render_summary_table(notes: Dict[str, Note], groups: List[TxGroup]) -> str:
    """Render a summary table of all UTXOs."""
    lines: List[str] = []
    
    lines.append("")
    lines.append("╔═══════════════════════════════════════════════════════════════════════════════════════════════════════════╗")
    lines.append("║                                         UTXO SUMMARY TABLE                                                ║")
    lines.append("╠═══════════════════════════════════════════════════════════════════════════════════════════════════════════╣")
    
    # Header
    lines.append("║ CM (short)          │ Value  │ Status  │ To                       │ From                     ║")
    lines.append("╠═════════════════════╪════════╪═════════╪══════════════════════════╪══════════════════════════╣")
    
    # Sort notes by whether they're spent, then by cm
    sorted_notes = sorted(notes.values(), key=lambda n: (n.spent, n.cm))
    
    for note in sorted_notes:
        cm_short = _short_hex(note.cm, 6, 6)
        value = note.value or "?"
        status = "SPENT" if note.spent else "LIVE"
        to_short = _short_addr(note.recipient or "", 6, 6)
        from_short = _short_addr(note.sender_id or "", 6, 6)
        
        lines.append(f"║ {cm_short:<19} │ {value:<6} │ {status:<7} │ {to_short:<24} │ {from_short:<24} ║")
    
    lines.append("╚═══════════════════════════════════════════════════════════════════════════════════════════════════════════╝")
    
    # Stats
    total = len(notes)
    live = sum(1 for n in notes.values() if not n.spent)
    spent = total - live
    
    lines.append("")
    lines.append(f"  📊 Statistics: {total} total UTXOs | 🟢 {live} LIVE | 🔴 {spent} SPENT")
    lines.append("")
    
    return "\n".join(lines)


def _render_compact_flow(
    notes: Dict[str, Note],
    groups: List[TxGroup],
    *,
    show_values: bool = True,
) -> str:
    """Render a compact horizontal flow view."""
    lines: List[str] = []
    
    lines.append("")
    lines.append("╔════════════════════════════════════════════════════════════════════════════════╗")
    lines.append("║                          COMPACT TRANSACTION FLOW                              ║")
    lines.append("╚════════════════════════════════════════════════════════════════════════════════╝")
    lines.append("")
    
    for i, group in enumerate(groups):
        inputs = sorted(group.inputs)
        outputs = group.outputs
        kind = group.kind or "?"
        tx_short = _short_hex(group.tx_hash, 4, 4)
        
        # Format inputs
        if inputs:
            in_strs = []
            for cm_in in inputs[:3]:  # Max 3 inputs shown
                note = notes.get(cm_in)
                val = f"({note.value})" if note and note.value and show_values else ""
                in_strs.append(f"[{_short_hex(cm_in, 4, 4)}{val}]")
            in_part = " + ".join(in_strs)
            if len(inputs) > 3:
                in_part += f" +{len(inputs)-3}..."
        else:
            in_part = "[MINT ∅]"
        
        # Format outputs  
        out_strs = []
        for out in outputs[:3]:  # Max 3 outputs shown
            val = f"({out.value})" if out.value and show_values else ""
            status = "🟢" if not out.spent else "🔴"
            out_strs.append(f"{status}[{_short_hex(out.cm, 4, 4)}{val}]")
        out_part = " + ".join(out_strs)
        if len(outputs) > 3:
            out_part += f" +{len(outputs)-3}..."
        
        # Render flow line
        lines.append(f"  {in_part}")
        lines.append(f"        │")
        lines.append(f"        ▼")
        lines.append(f"   ┌──────────────────┐")
        lines.append(f"   │ ⚡ {kind.upper():<12} │")
        lines.append(f"   │    {tx_short:<12} │")
        lines.append(f"   └──────────────────┘")
        lines.append(f"        │")
        lines.append(f"        ▼")
        lines.append(f"  {out_part}")
        lines.append("")
        lines.append("  " + "═" * 60)
        lines.append("")
    
    return "\n".join(lines)


def main(argv: Optional[Sequence[str]] = None) -> int:
    p = argparse.ArgumentParser(
        description="Render ASCII UTXO flow diagrams from notes_nullifiers JSON",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s notes.json                    # Basic diagram
  %(prog)s notes.json --values           # Include values
  %(prog)s notes.json --paths --values   # Show chain paths with values
  %(prog)s notes.json --summary          # Show summary table
  %(prog)s notes.json --compact          # Compact horizontal flow
  %(prog)s notes.json --all              # Everything
        """
    )
    p.add_argument("path", nargs="?", default=None, help="Path to JSON file, or '-' for stdin")
    p.add_argument("--input", "-i", default="-", help="Path to JSON file (deprecated; use positional)")
    p.add_argument("--values", action="store_true", help="Show values in UTXO boxes")
    p.add_argument("--addresses", action="store_true", help="Show recipient/sender addresses")
    p.add_argument("--paths", action="store_true", help="Show UTXO chain paths diagram")
    p.add_argument("--summary", action="store_true", help="Show summary table")
    p.add_argument("--compact", action="store_true", help="Show compact horizontal flow")
    p.add_argument("--all", action="store_true", help="Show all visualizations")
    p.add_argument("--max-paths", type=int, default=20, help="Max chain paths to show")
    args = p.parse_args(argv)

    try:
        input_path = args.path if args.path is not None else args.input
        rows = _read_rows(input_path)
    except Exception as e:
        sys.stderr.write(f"error: failed to read/parse input: {e}\n")
        return 2

    notes = _build_notes(rows)
    groups = _build_tx_groups(notes)

    if not notes:
        sys.stdout.write("No notes found in input.\n")
        return 0

    show_all = args.all
    show_values = args.values or show_all
    show_addresses = args.addresses or show_all

    # Default: show compact flow if nothing specific requested
    if not (args.paths or args.summary or args.compact or show_all):
        args.compact = True

    if args.summary or show_all:
        sys.stdout.write(_render_summary_table(notes, groups))

    if args.compact or show_all:
        sys.stdout.write(_render_compact_flow(notes, groups, show_values=show_values))

    if args.paths or show_all:
        sys.stdout.write(_render_chain_diagram(
            notes,
            show_values=show_values,
            show_addresses=show_addresses,
            max_chains=args.max_paths,
        ))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
