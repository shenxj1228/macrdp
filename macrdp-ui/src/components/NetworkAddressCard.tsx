import { useCallback, useEffect, useRef, useState } from "react";
import { Check, Copy, RefreshCw } from "lucide-react";
import { api } from "../lib/ipc";
import type { LocalAddress } from "../lib/types";

interface NetworkAddressCardProps {
  port: number;
}

const kindLabel: Record<LocalAddress["kind"], string> = {
  lan: "局域网",
  vpn: "VPN",
  other: "其他",
};

const kindClass: Record<LocalAddress["kind"], string> = {
  lan: "bg-green/10 text-green",
  vpn: "bg-accent/10 text-accent",
  other: "bg-text-muted/10 text-text-muted",
};

async function copyText(text: string) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return;
    }
  } catch {
    // Fall through to the legacy path (no secure context / clipboard permission).
  }
  const el = document.createElement("textarea");
  el.value = text;
  el.style.position = "fixed";
  el.style.opacity = "0";
  document.body.appendChild(el);
  el.select();
  document.execCommand("copy");
  document.body.removeChild(el);
}

function NetworkAddressCard({ port }: NetworkAddressCardProps) {
  const [addresses, setAddresses] = useState<LocalAddress[]>([]);
  const [copied, setCopied] = useState<string | null>(null);
  const timer = useRef<number | null>(null);

  const load = useCallback(() => {
    api.getLocalAddresses().then(setAddresses).catch(console.error);
  }, []);

  useEffect(() => {
    load();
    // A VPN reconnect assigns a new address, so refresh periodically and on focus.
    const interval = window.setInterval(load, 5000);
    window.addEventListener("focus", load);
    return () => {
      window.clearInterval(interval);
      window.removeEventListener("focus", load);
      if (timer.current !== null) window.clearTimeout(timer.current);
    };
  }, [load]);

  const handleCopy = async (address: LocalAddress) => {
    const key = `${address.interface}-${address.address}`;
    try {
      await copyText(`${address.address}:${port}`);
      setCopied(key);
      if (timer.current !== null) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(null), 1500);
    } catch (err) {
      console.error("Failed to copy address:", err);
    }
  };

  const hasVpn = addresses.some((a) => a.kind === "vpn");

  return (
    <div className="bg-card rounded-[8px] p-3">
      <div className="flex items-center gap-2 mb-2">
        <span className="text-[11px] text-text-muted font-medium">本机地址</span>
        <span className="text-[10px] text-text-muted/70">
          复制后在客户端使用
        </span>
        <button
          onClick={load}
          className="ml-auto text-text-muted hover:text-text transition-colors"
          title="刷新"
          aria-label="刷新地址"
        >
          <RefreshCw className="h-3 w-3" />
        </button>
      </div>

      {addresses.length > 0 ? (
        <div className="flex flex-col gap-1">
          {addresses.map((address) => {
            const key = `${address.interface}-${address.address}`;
            return (
              <div key={key} className="flex items-center gap-2">                <span
                  className={`shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium ${kindClass[address.kind]}`}
                >
                  {kindLabel[address.kind]}
                </span>
                <span className="w-[52px] shrink-0 text-[10px] text-text-muted">
                  {address.interface}
                </span>
                <code className="min-w-0 flex-1 truncate font-mono text-xs">
                  {address.address}:{port}
                </code>
                <button
                  onClick={() => handleCopy(address)}
                  className="inline-flex shrink-0 items-center gap-1 rounded-md px-1.5 py-0.5 text-[10px] text-text-muted transition-colors hover:bg-bg hover:text-text"
                  title="复制地址"
                >
                  {copied === key ? (
                    <>
                      <Check className="h-3 w-3 text-green" />
                      已复制
                    </>
                  ) : (
                    <>
                      <Copy className="h-3 w-3" />
                      复制
                    </>
                  )}
                </button>
              </div>
            );
          })}
        </div>
      ) : (
        <div className="py-1 text-xs text-text-muted">未检测到可用地址</div>
      )}

      {hasVpn && (
        <div className="mt-2 text-[10px] text-text-muted/70">
          VPN 地址在每次重连后可能变化，以这里当前显示的为准。
        </div>
      )}
    </div>
  );
}

export default NetworkAddressCard;
