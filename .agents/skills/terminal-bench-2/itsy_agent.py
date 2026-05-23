"""Harbor adapter for the itsy agent.

Uploads a prebuilt `itsy` binary into the bench task container and runs
it in one-shot non-interactive mode against the task instruction.

Lives next to the `terminal-bench-2` skill's SKILL.md. Because the
parent directory contains a hyphen (a Python module-name violation),
callers point `PYTHONPATH` at this directory so `itsy_agent` imports
as a flat module.

Usage (from the itsy repo root):

    ITSY_BINARY=$PWD/target/x86_64-unknown-linux-musl/release/itsy \\
    PYTHONPATH=$PWD/.agents/skills/terminal-bench-2 \\
    uv run --with harbor harbor run \\
        --dataset terminal-bench@2.0 \\
        --agent-import-path itsy_agent:ItsyAgent \\
        --model unsloth/Qwen3.6-35B-A3B-GGUF:IQ2_XXS \\
        --include-task-name fix-git --n-attempts 1 --n-concurrent 1

See `SKILL.md` in this directory for the full configuration
walkthrough, prerequisites, and result interpretation.
"""

from __future__ import annotations

import os
import shlex
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class ItsyAgent(BaseInstalledAgent):
    """itsy — Rust port of smallcode, optimized for small local LLMs."""

    SUPPORTS_ATIF: bool = False
    SUPPORTS_WINDOWS: bool = False

    _OUTPUT_FILENAME = "itsy.txt"
    _BINARY_TARGET = "/usr/local/bin/itsy"

    @staticmethod
    def name() -> str:
        return "itsy"

    def get_version_command(self) -> str | None:
        return f"{self._BINARY_TARGET} --help 2>&1 | head -1 || true"

    def parse_version(self, stdout: str) -> str:
        return "0.1.0"

    async def install(self, environment: BaseEnvironment) -> None:
        """Upload the prebuilt itsy binary + ensure deps exist in container."""
        binary = Path(os.environ.get("ITSY_BINARY", "/workspace/itsy/target/release/itsy"))
        if not binary.exists():
            raise RuntimeError(
                f"ITSY_BINARY not found at {binary}. Build itsy first with "
                "`cargo build --release` in /workspace/itsy."
            )

        # Some bench images ship slim debian. ca-certificates + curl are
        # routinely needed for itsy's HTTP calls; install them up front so
        # the agent doesn't waste tool calls discovering missing deps.
        await self.exec_as_root(
            environment,
            command=(
                "set -e; "
                "if command -v apt-get >/dev/null 2>&1; then "
                "  DEBIAN_FRONTEND=noninteractive apt-get update -qq && "
                "  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "
                "    ca-certificates curl git >/dev/null 2>&1 || true; "
                "elif command -v apk >/dev/null 2>&1; then "
                "  apk add --no-cache ca-certificates curl git >/dev/null 2>&1 || true; "
                "fi"
            ),
        )

        await environment.upload_file(binary, self._BINARY_TARGET)
        await self.exec_as_root(
            environment,
            command=f"chmod +x {shlex.quote(self._BINARY_TARGET)}",
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        base_url = os.environ.get("ITSY_BASE_URL", "http://10.0.2.2:8000/v1")
        model = self.model_name or os.environ.get("ITSY_MODEL", "")
        if not model:
            raise ValueError(
                "Model name required. Pass --model or set ITSY_MODEL env var."
            )

        # ITSY_HOME is the lone env var itsy still consults — it resolves
        # before the config file does, so it has to be an env var by
        # construction. Every other knob is a CLI flag.
        env: dict[str, str] = {"ITSY_HOME": "/tmp/itsy-home"}

        escaped = shlex.quote(instruction)
        # CLI flags supplant the old ITSY_* env-var bridge. Order matters:
        # named flags first, then --set key=value for the long tail.
        flags = [
            f"--model={shlex.quote(model)}",
            f"--endpoint={shlex.quote(base_url)}",
            "--provider=openai",
            "--auto-approve",
            "--classic",
            "--non-interactive",
            # Generous timeouts — small models on tight quants take a
            # while per turn; bench tasks have their own outer timeouts.
            "--bash-timeout=120",
            "--request-timeout-ms=600000",
            # Reasoning headroom — IQ2_XXS will exhaust output budget on
            # thinking unless max_output_tokens > thinking_budget.
            "--thinking-budget=8000",
            # Safeguards on by default — cheap and catch patch/bash/
            # write_guard pitfalls. Disable extras that have no upstream
            # equivalent (reviewer, chain, clarifier, contract).
            "--set=features.reviewer=false",
            "--set=features.chain=false",
            "--set=features.clarifier=false",
        ]
        cmd = (
            f"mkdir -p /tmp/itsy-home /logs/agent && "
            f"{self._BINARY_TARGET} {' '.join(flags)} -p {escaped} "
            f"2>&1 </dev/null | tee /logs/agent/{self._OUTPUT_FILENAME}"
        )
        await self.exec_as_agent(environment, command=cmd, env=env)

    def populate_context_post_run(self, context: AgentContext) -> None:
        out = self.logs_dir / self._OUTPUT_FILENAME
        if out.exists():
            context.metadata = context.metadata or {}
