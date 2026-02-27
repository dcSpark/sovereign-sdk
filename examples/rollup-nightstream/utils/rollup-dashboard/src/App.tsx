import { useState, useEffect, useCallback } from 'react';
import type { HealthResponse, ActionType, ActionResult, EmaMetricsResponse, EmaWindow } from './types';
import { fetchHealth, performAction, fetchEmaMetrics } from './api';
import { SystemStatsPanel } from './SystemStats';
import { Terminal } from './Terminal';
import './styles.css';

interface RowProps {
  title: string;
  children: React.ReactNode;
  defaultExpanded?: boolean;
  badge?: string;
  badgeColor?: 'healthy' | 'unhealthy' | 'warning' | 'neutral';
}

function DashboardRow({ title, children, defaultExpanded = true, badge, badgeColor = 'neutral' }: RowProps) {
  const [expanded, setExpanded] = useState(defaultExpanded);
  
  return (
    <div className={`dashboard-row ${expanded ? 'expanded' : 'collapsed'}`}>
      <button className="row-header" onClick={() => setExpanded(!expanded)}>
        <span className="row-chevron">{expanded ? '▼' : '▶'}</span>
        <span className="row-title">{title}</span>
        {badge && (
          <span className={`row-badge badge-${badgeColor}`}>{badge}</span>
        )}
      </button>
      {expanded && <div className="row-content">{children}</div>}
    </div>
  );
}

const EMA_WINDOW_OPTIONS: Array<{ value: EmaWindow; label: string }> = [
  { value: 's2', label: '2s' },
  { value: 's5', label: '5s' },
  { value: 'm1', label: '1m' },
  { value: 'm5', label: '5m' },
  { value: 'm15', label: '15m' },
];

function App() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [l2Metrics, setL2Metrics] = useState<EmaMetricsResponse | null>(null);
  const [l2MetricsWindow, setL2MetricsWindow] = useState<EmaWindow>('m1');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [l2MetricsError, setL2MetricsError] = useState<string | null>(null);
  const [actionLoadingKey, setActionLoadingKey] = useState<string | null>(null);
  const [actionResult, setActionResult] = useState<ActionResult | null>(null);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [refreshInterval, setRefreshInterval] = useState(5000);

  const loadHealth = useCallback(async () => {
    try {
      const data = await fetchHealth();
      setHealth(data);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch health');
    } finally {
      setLoading(false);
    }
  }, []);

  const loadL2Metrics = useCallback(async () => {
    try {
      const data = await fetchEmaMetrics(l2MetricsWindow);
      setL2Metrics(data);
      setL2MetricsError(data ? null : 'EMA metrics unavailable');
    } catch (err) {
      setL2Metrics(null);
      setL2MetricsError(err instanceof Error ? err.message : 'Failed to fetch EMA metrics');
    }
  }, [l2MetricsWindow]);

  useEffect(() => {
    loadHealth();
    loadL2Metrics();
    if (autoRefresh) {
      const interval = setInterval(() => {
        loadHealth();
        loadL2Metrics();
      }, refreshInterval);
      return () => clearInterval(interval);
    }
  }, [loadHealth, loadL2Metrics, autoRefresh, refreshInterval]);

  const actionKey = (action: ActionType, serviceId?: string) => `${action}:${serviceId ?? 'all'}`;

  const handleAction = async (action: ActionType, serviceId?: string) => {
    const key = actionKey(action, serviceId);
    setActionLoadingKey(key);
    setActionResult(null);
    const result = await performAction(action, serviceId);
    setActionResult(result);
    setActionLoadingKey(null);
    setTimeout(loadHealth, 1000);
  };

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'healthy':
        return 'status-healthy';
      case 'unhealthy':
        return 'status-unhealthy';
      default:
        return 'status-unknown';
    }
  };

  const formatResponseTime = (ms?: number) => {
    if (ms === undefined) return '-';
    return `${ms.toFixed(0)}ms`;
  };

  const formatProcessState = (running: boolean, pid?: number, remote?: boolean) => {
    if (remote) return 'remote';
    if (!running) return 'stopped';
    return pid ? `managed (pid ${pid})` : 'managed';
  };

  const formatNumber = (value: number | undefined | null, decimals: number = 2): string => {
    if (value === undefined || value === null || typeof value !== 'number' || !isFinite(value)) return '-';
    if (Math.abs(value) >= 1_000_000) {
      return `${(value / 1_000_000).toFixed(decimals)}M`;
    }
    if (Math.abs(value) >= 1_000) {
      return `${(value / 1_000).toFixed(decimals)}K`;
    }
    return value.toFixed(decimals);
  };

  const formatTps = (value: number | undefined | null): string => {
    if (value === undefined || value === null || typeof value !== 'number' || !isFinite(value)) return '-';
    return value.toFixed(3);
  };

  const healthyCount = health?.services.filter(s => s.status === 'healthy').length ?? 0;
  const totalCount = health?.services.length ?? 0;
  const hasRunningServices = health?.services.some((service) => service.running) ?? false;
  const replicaMode = health?.replicaMode ?? false;
  const servicesStatusText = `${healthyCount}/${totalCount} healthy`;
  const servicesStatusColor = healthyCount === totalCount ? 'healthy' : healthyCount === 0 ? 'unhealthy' : 'warning';

  return (
    <div className="app grafana-style">
      {/* Top Navigation Bar */}
      <header className="dashboard-navbar">
        <div className="navbar-left">
          <div className="dashboard-logo">
            <span className="logo-icon">◈</span>
            <span className="logo-text">Midnight L2</span>
          </div>
          <div className="navbar-divider" />
          <h1 className="dashboard-title">Service Dashboard</h1>
        </div>
        <div className="navbar-center">
          {health && (
            <div className={`system-status-pill ${getStatusColor(health.status)}`}>
              <span className="status-dot-small" />
              <span>System {health.status}</span>
            </div>
          )}
        </div>
        <div className="navbar-right">
          <div className="refresh-controls">
            <select 
              className="refresh-interval-select"
              value={refreshInterval}
              onChange={(e) => setRefreshInterval(Number(e.target.value))}
            >
              <option value={5000}>5s</option>
              <option value={10000}>10s</option>
              <option value={30000}>30s</option>
              <option value={60000}>1m</option>
            </select>
            <label className="auto-refresh-toggle">
              <input
                type="checkbox"
                checked={autoRefresh}
                onChange={(e) => setAutoRefresh(e.target.checked)}
              />
              <span className="toggle-slider" />
            </label>
            <button className="icon-btn refresh" onClick={() => { loadHealth(); loadL2Metrics(); }} disabled={loading}>
              <span className={loading ? 'spinning' : ''}>↻</span>
            </button>
          </div>
          {health && (
            <span className="last-updated">
              {new Date(health.checkedAt).toLocaleTimeString()}
            </span>
          )}
        </div>
      </header>

      <main className="dashboard-main">
        {error ? (
          <div className="error-panel">
            <div className="error-icon">⚠</div>
            <div className="error-content">
              <strong>Connection Error</strong>
              <p>Service controller unreachable at <code>http://127.0.0.1:9090</code></p>
            </div>
          </div>
        ) : loading && !health ? (
          <div className="loading-panel">
            <div className="loading-spinner" />
            <span>Loading dashboard...</span>
          </div>
        ) : health ? (
          <>
            {/* Row: Service Controls */}
            <DashboardRow title="Service Controls" defaultExpanded={true}>
              <div className="controls-panel">
                <div className="controls-grid">
                  <button
                    className="control-btn start"
                    onClick={() => handleAction('start')}
                    disabled={actionLoadingKey !== null}
                  >
                    <span className="btn-icon">▶</span>
                    {actionLoadingKey === actionKey('start') ? 'Starting...' : 'Start All'}
                  </button>
                  <button
                    className="control-btn stop"
                    onClick={() => handleAction('stop')}
                    disabled={actionLoadingKey !== null}
                  >
                    <span className="btn-icon">■</span>
                    {actionLoadingKey === actionKey('stop') ? 'Stopping...' : 'Stop All'}
                  </button>
                  <button
                    className="control-btn restart"
                    onClick={() => handleAction('restart')}
                    disabled={actionLoadingKey !== null}
                  >
                    <span className="btn-icon">↻</span>
                    {actionLoadingKey === actionKey('restart') ? 'Restarting...' : 'Restart All'}
                  </button>
                  {!replicaMode && (
                    <button
                      className="control-btn clean"
                      onClick={() => handleAction('clean')}
                      disabled={actionLoadingKey !== null}
                    >
                      <span className="btn-icon">🗑</span>
                      {actionLoadingKey === actionKey('clean') ? 'Cleaning...' : 'Clean Data'}
                    </button>
                  )}
                  {!replicaMode && (
                    <button
                      className="control-btn clean-db"
                      onClick={() => handleAction('clean-database')}
                      disabled={actionLoadingKey !== null || hasRunningServices}
                    >
                      <span className="btn-icon">⌫</span>
                      {actionLoadingKey === actionKey('clean-database') ? 'Cleaning...' : 'Clean Database'}
                    </button>
                  )}
                  {!replicaMode && (
                    <button
                      className="control-btn reset-tee"
                      onClick={() => handleAction('reset-tee')}
                      disabled={actionLoadingKey !== null || hasRunningServices}
                    >
                      <span className="btn-icon">⟲</span>
                      {actionLoadingKey === actionKey('reset-tee') ? 'Resetting...' : 'Reset TEE'}
                    </button>
                  )}
                  {!replicaMode && (
                    <button
                      className="control-btn reset-tee"
                      onClick={() => handleAction('reset-replica')}
                      disabled={actionLoadingKey !== null}
                    >
                      <span className="btn-icon">⟲</span>
                      {actionLoadingKey === actionKey('reset-replica') ? 'Resetting...' : 'Reset Replica'}
                    </button>
                  )}
                </div>
                {actionResult && (
                  <div className={`action-toast ${actionResult.success ? 'success' : 'error'}`}>
                    {actionResult.success ? '✓' : '✗'} {actionResult.message}
                  </div>
                )}
              </div>
            </DashboardRow>

            {/* Row: L2 Metrics */}
            <DashboardRow title="L2 Metrics" defaultExpanded={true}>
              <div className="charts-header">
                <div className="time-range-selector">
                  {EMA_WINDOW_OPTIONS.map((option) => (
                    <button
                      key={option.value}
                      className={`time-range-btn ${l2MetricsWindow === option.value ? 'active' : ''}`}
                      onClick={() => setL2MetricsWindow(option.value)}
                    >
                      {option.label}
                    </button>
                  ))}
                </div>
              </div>
              {l2MetricsError && (
                <div className="panel-error">EMA metrics unavailable: {l2MetricsError}</div>
              )}
              <div className="overview-panels">
                <div className="stat-panel highlight">
                  <div className="stat-panel-label">TPS</div>
                  <div className="stat-panel-value">{formatTps(l2Metrics?.TPS)}</div>
                  <div className="stat-panel-subtext">transactions/sec</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Peak TPS</div>
                  <div className="stat-panel-value">{formatTps(l2Metrics?.PeakTPS)}</div>
                  <div className="stat-panel-subtext">local peak</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Accounts</div>
                  <div className="stat-panel-value">{formatNumber(l2Metrics?.Accounts, 0)}</div>
                  <div className="stat-panel-subtext">total</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Sending Accounts</div>
                  <div className="stat-panel-value">{formatNumber(l2Metrics?.SendingAccounts, 0)}</div>
                  <div className="stat-panel-subtext">active senders</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Disclosure Events</div>
                  <div className="stat-panel-value">{formatNumber(l2Metrics?.TotalDisclosureEvents, 0)}</div>
                  <div className="stat-panel-subtext">cumulative</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Tokens In Wallets</div>
                  <div className="stat-panel-value">{formatNumber(l2Metrics?.TotalTokensInWallets, 0)}</div>
                  <div className="stat-panel-subtext">total tokens</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Total Transactions</div>
                  <div className="stat-panel-value">{formatNumber(l2Metrics?.TotalTransactions, 0)}</div>
                  <div className="stat-panel-subtext">cumulative</div>
                </div>
              </div>
            </DashboardRow>

            {/* Row: System Resources */}
            <DashboardRow title="System Resources" defaultExpanded={true}>
              <SystemStatsPanel autoRefresh={autoRefresh} />
            </DashboardRow>

            {/* Row: Services */}
            <DashboardRow 
              title="Services" 
              defaultExpanded={true}
              badge={servicesStatusText}
              badgeColor={servicesStatusColor}
            >
              <div className="services-panel">
                <div className="services-table">
                  <div className="table-header">
                    <div className="col-status">Status</div>
                    <div className="col-name">Service</div>
                    <div className="col-endpoint">Endpoint</div>
                    <div className="col-latency">Latency</div>
                    <div className="col-process">Process</div>
                    <div className="col-actions">Actions</div>
                    <div className="col-error">Error</div>
                  </div>
                  {health.services.map((service) => {
                    const controllable = service.controllable ?? !service.remote;
                    return (
                      <div key={service.id} className={`table-row ${getStatusColor(service.status)}`}>
                        <div className="col-status">
                          <span className="status-indicator-dot" />
                        </div>
                        <div className="col-name">{service.name}</div>
                        <div className="col-endpoint">
                          <code>{service.url}</code>
                        </div>
                        <div className="col-latency">{formatResponseTime(service.response_time_ms)}</div>
                        <div className="col-process">{formatProcessState(service.running, service.pid, service.remote)}</div>
                        <div className="col-actions">
                          {controllable ? (
                            <div className="service-actions">
                              <button
                                className="service-action-btn start"
                                onClick={() => handleAction('start', service.id)}
                                disabled={actionLoadingKey !== null || service.running}
                              >
                                {actionLoadingKey === actionKey('start', service.id) ? '...' : 'Start'}
                              </button>
                              <button
                                className="service-action-btn stop"
                                onClick={() => handleAction('stop', service.id)}
                                disabled={actionLoadingKey !== null || !service.running}
                              >
                                {actionLoadingKey === actionKey('stop', service.id) ? '...' : 'Stop'}
                              </button>
                              <button
                                className="service-action-btn restart"
                                onClick={() => handleAction('restart', service.id)}
                                disabled={actionLoadingKey !== null}
                              >
                                {actionLoadingKey === actionKey('restart', service.id) ? '...' : 'Restart'}
                              </button>
                            </div>
                          ) : (
                            <span className="service-action-note">Remote</span>
                          )}
                        </div>
                        <div className="col-error">{service.error || '-'}</div>
                      </div>
                    );
                  })}
                </div>
              </div>
            </DashboardRow>

            {/* Row: Service Logs */}
            <DashboardRow title="Service Logs" defaultExpanded={false}>
              <Terminal />
            </DashboardRow>
          </>
        ) : null}
      </main>

      <footer className="dashboard-footer">
        <span>Midnight L2 Service Controller</span>
        <code>http://127.0.0.1:9090</code>
      </footer>
    </div>
  );
}

export default App;
