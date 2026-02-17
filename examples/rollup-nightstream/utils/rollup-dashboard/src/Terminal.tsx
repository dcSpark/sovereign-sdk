import { useState, useEffect, useRef, useCallback } from 'react';

interface LogLine {
  timestamp: string;
  service: string;
  stream: 'stdout' | 'stderr';
  content: string;
}

interface TerminalProps {
  wsUrl?: string;
}

export function Terminal({ wsUrl = '/controller/logs' }: TerminalProps) {
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [connected, setConnected] = useState(false);
  const [autoScroll, setAutoScroll] = useState(true);
  const [streamFilter, setStreamFilter] = useState<'all' | 'stdout' | 'stderr'>('all');
  const [serviceFilter, setServiceFilter] = useState<string>('all');
  const terminalRef = useRef<HTMLDivElement>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const reconnectTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const connect = useCallback(() => {
    // Determine the WebSocket URL
    let fullWsUrl: string;
    if (wsUrl.startsWith('ws://') || wsUrl.startsWith('wss://')) {
      fullWsUrl = wsUrl;
    } else {
      const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
      const host = window.location.host;
      fullWsUrl = `${protocol}//${host}${wsUrl}`;
    }

    console.log('Connecting to WebSocket:', fullWsUrl);
    const ws = new WebSocket(fullWsUrl);
    wsRef.current = ws;

    ws.onopen = () => {
      console.log('WebSocket connected');
      setConnected(true);
    };

    ws.onmessage = (event) => {
      try {
        const logLine: LogLine = JSON.parse(event.data);
        setLogs((prev) => {
          // Keep last 5000 lines to prevent memory issues
          const newLogs = [...prev, logLine];
          if (newLogs.length > 5000) {
            return newLogs.slice(-5000);
          }
          return newLogs;
        });
      } catch (e) {
        console.error('Failed to parse log message:', e);
      }
    };

    ws.onclose = () => {
      console.log('WebSocket disconnected');
      setConnected(false);
      wsRef.current = null;

      // Attempt to reconnect after 3 seconds
      reconnectTimeoutRef.current = setTimeout(() => {
        console.log('Attempting to reconnect...');
        connect();
      }, 3000);
    };

    ws.onerror = (error) => {
      console.error('WebSocket error:', error);
    };
  }, [wsUrl]);

  useEffect(() => {
    connect();

    return () => {
      if (wsRef.current) {
        wsRef.current.close();
      }
      if (reconnectTimeoutRef.current) {
        clearTimeout(reconnectTimeoutRef.current);
      }
    };
  }, [connect]);

  // Auto-scroll to bottom when new logs arrive
  useEffect(() => {
    if (autoScroll && terminalRef.current) {
      terminalRef.current.scrollTop = terminalRef.current.scrollHeight;
    }
  }, [logs, autoScroll]);

  const clearLogs = () => {
    setLogs([]);
    setServiceFilter('all');
  };

  const availableServices = Array.from(
    new Set(logs.map((log) => (log.service?.trim() ? log.service : 'unknown')))
  ).sort((a, b) => a.localeCompare(b));

  if (serviceFilter !== 'all' && !availableServices.includes(serviceFilter)) {
    availableServices.unshift(serviceFilter);
  }

  const filteredLogs = logs.filter((log) => {
    const resolvedService = log.service?.trim() ? log.service : 'unknown';
    const matchesStream = streamFilter === 'all' || log.stream === streamFilter;
    const matchesService = serviceFilter === 'all' || resolvedService === serviceFilter;
    return matchesStream && matchesService;
  });

  const formatTimestamp = (timestamp: string) => {
    try {
      const date = new Date(timestamp);
      return date.toLocaleTimeString([], { 
        hour: '2-digit', 
        minute: '2-digit', 
        second: '2-digit',
        hour12: false 
      });
    } catch {
      return timestamp;
    }
  };

  return (
    <div className="terminal-section">
      <div className="terminal-header">
        <div className="terminal-title">
          <h2>Service Logs</h2>
          <span className={`connection-status ${connected ? 'connected' : 'disconnected'}`}>
            {connected ? '● Connected' : '○ Disconnected'}
          </span>
        </div>
        <div className="terminal-controls">
          <div className="filter-buttons">
            <button
              className={`filter-btn ${streamFilter === 'all' ? 'active' : ''}`}
              onClick={() => setStreamFilter('all')}
            >
              All
            </button>
            <button
              className={`filter-btn ${streamFilter === 'stdout' ? 'active' : ''}`}
              onClick={() => setStreamFilter('stdout')}
            >
              stdout
            </button>
            <button
              className={`filter-btn ${streamFilter === 'stderr' ? 'active' : ''}`}
              onClick={() => setStreamFilter('stderr')}
            >
              stderr
            </button>
          </div>
          <label className="service-filter-label">
            <span>Service</span>
            <select
              className="service-filter-select"
              value={serviceFilter}
              onChange={(e) => setServiceFilter(e.target.value)}
            >
              <option value="all">All services</option>
              {availableServices.map((service) => (
                <option key={service} value={service}>
                  {service}
                </option>
              ))}
            </select>
          </label>
          <label className="auto-scroll-toggle">
            <input
              type="checkbox"
              checked={autoScroll}
              onChange={(e) => setAutoScroll(e.target.checked)}
            />
            Auto-scroll
          </label>
          <button className="clear-btn" onClick={clearLogs}>
            Clear
          </button>
        </div>
      </div>
      <div className="terminal-container" ref={terminalRef}>
        {filteredLogs.length === 0 ? (
          <div className="terminal-empty">
            {connected 
              ? 'Waiting for logs... Start services to see output.'
              : 'Connecting to log stream...'}
          </div>
        ) : (
          filteredLogs.map((log, index) => (
            <div key={index} className={`log-line ${log.stream}`}>
              <span className="log-timestamp">{formatTimestamp(log.timestamp)}</span>
              <span className="log-service">{log.service || '-'}</span>
              <span className={`log-stream ${log.stream}`}>{log.stream}</span>
              <span className="log-content">{log.content}</span>
            </div>
          ))
        )}
      </div>
      <div className="terminal-footer">
        <span className="log-count">{filteredLogs.length} lines</span>
      </div>
    </div>
  );
}
