"""Harbor adapter for the itsy agent.

Uploads a prebuilt `itsy` binary into the bench task container, configures
it to reach the host's llama-server via the socat hop on claude-sandbox's
loopback (10.0.2.2:8000 from inside the task container resolves there),
and runs the agent in one-shot mode against the task instruction.

Usage (from the itsy repo root):

    ITSY_BINARY=$PWD/target/release/itsy \\
    ITSY_BASE_URL=http://10.0.2.2:8000/v1 \\
    ITSY_MODEL=unsloth/Qwen3.6-35B-A3B-GGUF:IQ2_XXS \\
    uv run --with harbor harbor run \\
        --dataset terminal-bench@2.0 \\
        --agent-import-path bench.itsy_agent:ItsyAgent \\
        --task fix-git --n-attempts 1 --n-concurrent 1
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

        # All state under /tmp/itsy-home so the container's filesystem stays
        # untouched outside the task's working directory.
        env: dict[str, str] = {
            "ITSY_HOME": "/tmp/itsy-home",
            "ITSY_BASE_URL": base_url,
            "ITSY_MODEL": model,
            "ITSY_PROVIDER": "openai",
            "ITSY_AUTO_APPROVE": "true",
            # Generous timeouts — small models on tight quants can take a
            # while per turn, and bench tasks have their own outer timeouts.
            "ITSY_MODEL_TIMEOUT": "600",
            "ITSY_BASH_TIMEOUT": "120",
            # Safeguards on by default — they're cheap and catch the
            # patch / bash / write_guard pitfalls that plague IQ2 quants.
            "ITSY_PLAN": "true",
            "ITSY_SNAPSHOT": "true",
            "ITSY_WRITE_GUARD": "true",
            "ITSY_BOOTSTRAP": "true",
            "ITSY_TRUST_DECAY": "true",
            "ITSY_TEMP_ADAPT": "true",
            "ITSY_SEMANTIC_MERGE": "true",
            "ITSY_ERROR_DIAGNOSIS": "true",
            "ITSY_CONTEXT_RETRIEVAL": "true",
            # No reviewer / chain / clarifier — they cost extra LLM round
            # trips and don't fit a one-shot non-interactive benchmark run.
            "ITSY_REVIEWER": "false",
            "ITSY_CHAIN": "false",
            "ITSY_CLARIFIER": "false",
            "ITSY_THINKING_BUDGET": "8000",
            # No tty inside docker exec — keep itsy in line mode.
            "ITSY_TUI_CLASSIC": "true",
        }

        escaped = shlex.quote(instruction)
        await self.exec_as_agent(
            environment,
            command=(
                f"mkdir -p /tmp/itsy-home /logs/agent && "
                f"{self._BINARY_TARGET} --classic --non-interactive -p {escaped} "
                f"2>&1 </dev/null | tee /logs/agent/{self._OUTPUT_FILENAME}"
            ),
            env=env,
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        out = self.logs_dir / self._OUTPUT_FILENAME
        if out.exists():
            context.metadata = context.metadata or {}
