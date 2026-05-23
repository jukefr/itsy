"""Harbor adapter for the smallcode agent.

Installs Node.js + smallcode into the bench task container and runs it
non-interactively against the task instruction.

Usage (from the itsy repo root):

    PYTHONPATH=$PWD/.agents/skills/terminal-bench-2 \\
    uv run --with harbor harbor run \\
        --dataset terminal-bench@2.0 \\
        --agent-import-path smallcode_agent:SmallcodeAgent \\
        --model unsloth/Qwen3.6-35B-A3B-GGUF:IQ2_XXS \\
        --include-task-name fix-git --n-attempts 3 --n-concurrent 1
"""

from __future__ import annotations

import os
import shlex

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class SmallcodeAgent(BaseInstalledAgent):
    """smallcode — JS coding agent optimized for small local LLMs."""

    SUPPORTS_ATIF: bool = False
    SUPPORTS_WINDOWS: bool = False

    _OUTPUT_FILENAME = "smallcode.txt"

    @staticmethod
    def name() -> str:
        return "smallcode"

    def get_version_command(self) -> str | None:
        return "smallcode --version 2>&1 | head -1 || true"

    def parse_version(self, stdout: str) -> str:
        return stdout.strip() or "unknown"

    async def install(self, environment: BaseEnvironment) -> None:
        """Install Node.js + smallcode inside the bench container."""
        await self.exec_as_root(
            environment,
            command=(
                "set -e; "
                # Ensure curl exists before we try to use it.
                "if command -v apt-get >/dev/null 2>&1; then "
                "  DEBIAN_FRONTEND=noninteractive apt-get update -qq >/dev/null 2>&1 && "
                "  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "
                "    ca-certificates curl git >/dev/null 2>&1 || true; "
                "elif command -v apk >/dev/null 2>&1; then "
                "  apk add --no-cache ca-certificates curl git >/dev/null 2>&1 || true; "
                "fi; "
                # Install Node.js 22 via nodesource if not already present.
                "if ! command -v node >/dev/null 2>&1; then "
                "  if command -v apt-get >/dev/null 2>&1; then "
                "    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null 2>&1 && "
                "    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nodejs >/dev/null 2>&1; "
                "  elif command -v apk >/dev/null 2>&1; then "
                "    apk add --no-cache nodejs npm >/dev/null 2>&1; "
                "  fi; "
                "fi; "
                "npm install -g smallcode >/dev/null 2>&1"
            ),
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        base_url = os.environ.get("SMALLCODE_BASE_URL", "http://10.0.2.2:8000/v1")
        model = self.model_name or os.environ.get("SMALLCODE_MODEL", "")
        if not model:
            raise ValueError("Model name required. Pass --model or set SMALLCODE_MODEL.")

        escaped = shlex.quote(instruction)
        cmd = (
            "mkdir -p /logs/agent && "
            f"smallcode --non-interactive "
            f"--provider openai "
            f"--endpoint {shlex.quote(base_url)} "
            f"--model {shlex.quote(model)} "
            f"-P {escaped} "
            f"2>&1 </dev/null | tee /logs/agent/{self._OUTPUT_FILENAME}"
        )
        await self.exec_as_agent(environment, command=cmd)

    def populate_context_post_run(self, context: AgentContext) -> None:
        out = self.logs_dir / self._OUTPUT_FILENAME
        if out.exists():
            context.metadata = context.metadata or {}
            context.metadata["agent_log"] = out.read_text(errors="replace")[-8000:]
