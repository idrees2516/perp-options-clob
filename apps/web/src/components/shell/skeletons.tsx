export function MarketsSkeleton() {
  return (
    <div className="h-full w-full p-3 flex flex-col gap-3">
      <div className="h-9 w-full shimmer rounded-lg" />
      <div className="flex-1 shimmer rounded-lg" />
    </div>
  );
}

export function PanelSkeleton({ className }: { className?: string }) {
  return <div className={`shimmer rounded-lg ${className ?? "h-full w-full"}`} />;
}
