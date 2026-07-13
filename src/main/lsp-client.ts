import { ipcMain, BrowserWindow } from 'electron';
import { spawn, ChildProcess } from 'child_process';
import * as path from 'path';
import {
  StreamMessageReader,
  StreamMessageWriter,
} from 'vscode-jsonrpc/node';
import {
  createProtocolConnection,
  ProtocolConnection,
  InitializeRequest,
  InitializedNotification,
  ShutdownRequest,
  ExitNotification,
  CompletionRequest,
  HoverRequest,
  DefinitionRequest,
  ReferencesRequest,
  DidOpenTextDocumentNotification,
  DidChangeTextDocumentNotification,
  DidCloseTextDocumentNotification,
  PublishDiagnosticsNotification,
  TextDocumentSyncKind,
  CompletionTriggerKind,
} from 'vscode-languageserver-protocol';
import { IPC } from '../shared/types';

export class LspClient {
  private process: ChildProcess | null = null;
  private connection: ProtocolConnection | null = null;
  private initialized = false;
  private rootUri: string | null = null;

  async initialize(rootPath: string): Promise<void> {
    // Kill existing instance
    this.shutdown();

    this.rootUri = `file://${rootPath}`;

    // Spawn clangd
    this.process = spawn('clangd', [
      '--background-index',
      '--clang-tidy',
      '--completion-style=detailed',
      '--header-insertion=iwyu',
      '--pch-storage=memory',
    ], {
      cwd: rootPath,
      stdio: ['pipe', 'pipe', 'pipe'],
    });

    this.process.stderr?.on('data', (data: Buffer) => {
      // clangd logs to stderr — ignore unless debugging
      // console.log('[clangd]', data.toString());
    });

    this.process.on('exit', (code) => {
      console.log(`clangd exited with code ${code}`);
      this.initialized = false;
      this.connection = null;
    });

    // Create LSP connection
    const reader = new StreamMessageReader(this.process.stdout!);
    const writer = new StreamMessageWriter(this.process.stdin!);
    this.connection = createProtocolConnection(reader, writer);
    this.connection.listen();

    // Forge include path for idetools.h
    const forgeIncludeDir = path.join(__dirname, '..', '..', '..', 'include');

    // Send initialize
    const initResult = await this.connection.sendRequest(InitializeRequest.type, {
      processId: process.pid,
      rootUri: this.rootUri,
      initializationOptions: {
        fallbackFlags: [`-std=c++17`, `-I${forgeIncludeDir}`],
      },
      capabilities: {
        textDocument: {
          completion: {
            completionItem: {
              snippetSupport: true,
              commitCharactersSupport: true,
              documentationFormat: ['markdown', 'plaintext'],
              resolveSupport: { properties: ['documentation', 'detail'] },
            },
            contextSupport: true,
          },
          hover: {
            contentFormat: ['markdown', 'plaintext'],
          },
          definition: { linkSupport: false },
          references: {},
          publishDiagnostics: {
            relatedInformation: true,
          },
          synchronization: {
            didSave: true,
            willSave: false,
            willSaveWaitUntil: false,
            dynamicRegistration: false,
          },
        },
        workspace: {
          workspaceFolders: true,
        },
      },
      workspaceFolders: [{
        uri: this.rootUri,
        name: rootPath.split('/').pop() || 'workspace',
      }],
    });

    // Send initialized notification
    this.connection.sendNotification(InitializedNotification.type, {});
    this.initialized = true;
  }

  async completion(uri: string, line: number, character: number): Promise<any> {
    if (!this.connection || !this.initialized) return { items: [] };

    try {
      const result = await this.connection.sendRequest(CompletionRequest.type, {
        textDocument: { uri },
        position: { line, character },
        context: { triggerKind: CompletionTriggerKind.Invoked },
      });

      if (!result) return { items: [] };
      return Array.isArray(result) ? { items: result } : result;
    } catch {
      return { items: [] };
    }
  }

  async hover(uri: string, line: number, character: number): Promise<any> {
    if (!this.connection || !this.initialized) return null;

    try {
      return await this.connection.sendRequest(HoverRequest.type, {
        textDocument: { uri },
        position: { line, character },
      });
    } catch {
      return null;
    }
  }

  async definition(uri: string, line: number, character: number): Promise<any[]> {
    if (!this.connection || !this.initialized) return [];

    try {
      const result = await this.connection.sendRequest(DefinitionRequest.type, {
        textDocument: { uri },
        position: { line, character },
      });

      if (!result) return [];
      return Array.isArray(result) ? result : [result];
    } catch {
      return [];
    }
  }

  async references(uri: string, line: number, character: number): Promise<any[]> {
    if (!this.connection || !this.initialized) return [];

    try {
      const result = await this.connection.sendRequest(ReferencesRequest.type, {
        textDocument: { uri },
        position: { line, character },
        context: { includeDeclaration: true },
      });

      return result || [];
    } catch {
      return [];
    }
  }

  didOpen(uri: string, content: string, version: number): void {
    if (!this.connection || !this.initialized) return;
    this.connection.sendNotification(DidOpenTextDocumentNotification.type, {
      textDocument: {
        uri,
        languageId: this.getLanguageId(uri),
        version,
        text: content,
      },
    });
  }

  didChange(uri: string, changes: any[], version: number): void {
    if (!this.connection || !this.initialized) return;
    this.connection.sendNotification(DidChangeTextDocumentNotification.type, {
      textDocument: { uri, version },
      contentChanges: changes,
    });
  }

  didClose(uri: string): void {
    if (!this.connection || !this.initialized) return;
    this.connection.sendNotification(DidCloseTextDocumentNotification.type, {
      textDocument: { uri },
    });
  }

  onDiagnostics(callback: (params: any) => void): void {
    if (!this.connection) return;
    this.connection.onNotification(
      PublishDiagnosticsNotification.type,
      callback
    );
  }

  shutdown(): void {
    if (this.connection && this.initialized) {
      try {
        this.connection.sendRequest(ShutdownRequest.type).catch(() => {});
        this.connection.sendNotification(ExitNotification.type);
      } catch {
        // clangd may already be gone
      }
    }
    if (this.process) {
      this.process.kill();
      this.process = null;
    }
    this.connection = null;
    this.initialized = false;
  }

  private getLanguageId(uri: string): string {
    if (uri.endsWith('.h') || uri.endsWith('.hpp') || uri.endsWith('.hxx')) return 'cpp';
    if (uri.endsWith('.c')) return 'c';
    if (uri.endsWith('.m')) return 'objective-c';
    if (uri.endsWith('.mm')) return 'objective-cpp';
    if (uri.endsWith('.cu')) return 'cuda';
    return 'cpp';
  }
}

export function registerLspHandlers(lsp: LspClient, win: BrowserWindow): void {
  ipcMain.handle(IPC.LSP_INITIALIZE, async (_e, rootPath: string) => {
    await lsp.initialize(rootPath);
    // Forward diagnostics to renderer
    lsp.onDiagnostics((params) => {
      if (!win.isDestroyed()) {
        win.webContents.send(IPC.LSP_DIAGNOSTICS, params);
      }
    });
  });

  ipcMain.handle(IPC.LSP_COMPLETION, async (_e, uri: string, line: number, char: number) => {
    return lsp.completion(uri, line, char);
  });

  ipcMain.handle(IPC.LSP_HOVER, async (_e, uri: string, line: number, char: number) => {
    return lsp.hover(uri, line, char);
  });

  ipcMain.handle(IPC.LSP_DEFINITION, async (_e, uri: string, line: number, char: number) => {
    return lsp.definition(uri, line, char);
  });

  ipcMain.handle(IPC.LSP_REFERENCES, async (_e, uri: string, line: number, char: number) => {
    return lsp.references(uri, line, char);
  });

  ipcMain.on(IPC.LSP_DID_OPEN, (_e, uri: string, content: string, version: number) => {
    lsp.didOpen(uri, content, version);
  });

  ipcMain.on(IPC.LSP_DID_CHANGE, (_e, uri: string, changes: any[], version: number) => {
    lsp.didChange(uri, changes, version);
  });

  ipcMain.on(IPC.LSP_DID_CLOSE, (_e, uri: string) => {
    lsp.didClose(uri);
  });

  // Memory info via hover — extract sizeof from hover response
  ipcMain.handle(IPC.LSP_MEMORY_INFO, async (_e, uri: string, line: number, char: number) => {
    const hover = await lsp.hover(uri, line, char);
    if (!hover) return null;

    const content = typeof hover.contents === 'string'
      ? hover.contents
      : (hover.contents as any).value || '';

    // clangd hover for types includes size info in some cases
    const sizeMatch = content.match(/sizeof\s*=\s*(\d+)/);
    return sizeMatch ? { size: parseInt(sizeMatch[1], 10), raw: content } : { raw: content };
  });
}
