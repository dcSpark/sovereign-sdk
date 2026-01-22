import { useState, useEffect, useCallback } from 'react';
import type { SystemStats } from './types';
import { fetchSystemStats } from './api';

interface SystemStatsProps {
  autoRefresh: boolean;
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i]}`;
}

function formatUptime(seconds: number): string {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  
  const parts = [];
  if (days > 0) parts.push(`${days}d`);
  if (hours > 0) parts.push(`${hours}h`);
  if (minutes > 0) parts.push(`${minutes}m`);
  
  return parts.length > 0 ? parts.join(' ') : '< 1m';
}

function ProgressBar({ value, color }: { value: number; color: string }) {
  const clampedValue = Math.min(100, Math.max(0, value));
  return (
    <div className="progress-bar-container">
      <div 
        className="progress-bar-fill" 
        style={{ 
          width: `${clampedValue}%`,
          backgroundColor: color,
        }} 
      />
      <span className="progress-bar-text">{clampedValue.toFixed(1)}%</span>
    </div>
  );
}

function getUsageColor(percent: number): string {
  if (percent < 50) return '#00ff88';
  if (percent < 80) return '#ffaa00';
  return '#ff3333';
}

export function SystemStatsPanel({ autoRefresh }: SystemStatsProps) {
  const [stats, setStats] = useState<SystemStats | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const loadStats = useCallback(async () => {
    try {
      const data = await fetchSystemStats();
      if (data) {
        setStats(data);
        setError(null);
      } else {
        setError('Failed to fetch system stats');
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Unknown error');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadStats();
    if (autoRefresh) {
      const interval = setInterval(loadStats, 5000);
      return () => clearInterval(interval);
    }
  }, [loadStats, autoRefresh]);

  if (loading && !stats) {
    return (
      <div className="system-stats-section">
        <div className="system-stats-loading">Loading system stats...</div>
      </div>
    );
  }

  if (error && !stats) {
    return (
      <div className="system-stats-section">
        <div className="system-stats-error">{error}</div>
      </div>
    );
  }

  if (!stats) {
    return null;
  }

  // Filter out tiny/virtual filesystems, keep only main disks
  const mainDisks = stats.disks.filter(
    (disk) =>
      disk.total_bytes > 1024 * 1024 * 1024 && // > 1GB
      !disk.mount_point.startsWith('/System') &&
      !disk.mount_point.includes('/Volumes/Recovery') &&
      disk.name !== 'devfs'
  );

  return (
    <div className="system-stats-section">
      <h2>System Resources</h2>
      <div className="system-stats-grid">
        {/* CPU Card */}
        <div className="stat-card">
          <div className="stat-card-header">
            <span className="stat-icon">🖥️</span>
            <h3>CPU</h3>
          </div>
          <div className="stat-value-large">{stats.cpu.usage_percent.toFixed(1)}%</div>
          <ProgressBar value={stats.cpu.usage_percent} color={getUsageColor(stats.cpu.usage_percent)} />
          <div className="stat-details">
            <div className="stat-detail">
              <span className="label">Cores</span>
              <span className="value">{stats.cpu.core_count}</span>
            </div>
            <div className="stat-detail">
              <span className="label">Load (1m)</span>
              <span className="value">{stats.load_average.one.toFixed(2)}</span>
            </div>
            <div className="stat-detail">
              <span className="label">Load (5m)</span>
              <span className="value">{stats.load_average.five.toFixed(2)}</span>
            </div>
          </div>
        </div>

        {/* Memory Card */}
        <div className="stat-card">
          <div className="stat-card-header">
            <span className="stat-icon">💾</span>
            <h3>Memory</h3>
          </div>
          <div className="stat-value-large">{stats.memory.usage_percent.toFixed(1)}%</div>
          <ProgressBar value={stats.memory.usage_percent} color={getUsageColor(stats.memory.usage_percent)} />
          <div className="stat-details">
            <div className="stat-detail">
              <span className="label">Used</span>
              <span className="value">{formatBytes(stats.memory.used_bytes)}</span>
            </div>
            <div className="stat-detail">
              <span className="label">Available</span>
              <span className="value">{formatBytes(stats.memory.available_bytes)}</span>
            </div>
            <div className="stat-detail">
              <span className="label">Total</span>
              <span className="value">{formatBytes(stats.memory.total_bytes)}</span>
            </div>
          </div>
        </div>

        {/* Uptime Card */}
        <div className="stat-card">
          <div className="stat-card-header">
            <span className="stat-icon">⏱️</span>
            <h3>Uptime</h3>
          </div>
          <div className="stat-value-large">{formatUptime(stats.uptime_seconds)}</div>
          <div className="stat-details">
            <div className="stat-detail">
              <span className="label">Load Avg</span>
              <span className="value">
                {stats.load_average.one.toFixed(2)} / {stats.load_average.five.toFixed(2)} / {stats.load_average.fifteen.toFixed(2)}
              </span>
            </div>
            {stats.memory.swap_total_bytes > 0 && (
              <div className="stat-detail">
                <span className="label">Swap</span>
                <span className="value">
                  {formatBytes(stats.memory.swap_used_bytes)} / {formatBytes(stats.memory.swap_total_bytes)}
                </span>
              </div>
            )}
          </div>
        </div>

        {/* Disk Cards */}
        {mainDisks.map((disk, index) => (
          <div key={index} className="stat-card">
            <div className="stat-card-header">
              <span className="stat-icon">💿</span>
              <h3>Disk {disk.mount_point}</h3>
            </div>
            <div className="stat-value-large">{disk.usage_percent.toFixed(1)}%</div>
            <ProgressBar value={disk.usage_percent} color={getUsageColor(disk.usage_percent)} />
            <div className="stat-details">
              <div className="stat-detail">
                <span className="label">Used</span>
                <span className="value">{formatBytes(disk.used_bytes)}</span>
              </div>
              <div className="stat-detail">
                <span className="label">Available</span>
                <span className="value">{formatBytes(disk.available_bytes)}</span>
              </div>
              <div className="stat-detail">
                <span className="label">Total</span>
                <span className="value">{formatBytes(disk.total_bytes)}</span>
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
