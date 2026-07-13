const path = require('path');
const HtmlWebpackPlugin = require('html-webpack-plugin');
const MonacoWebpackPlugin = require('monaco-editor-webpack-plugin');
const CopyWebpackPlugin = require('copy-webpack-plugin');

module.exports = {
  entry: './src/renderer/app.ts',
  target: 'web',
  output: {
    path: path.resolve(__dirname, 'dist/renderer'),
    filename: 'bundle.js',
    clean: true,
  },
  resolve: {
    extensions: ['.ts', '.js'],
  },
  module: {
    rules: [
      {
        test: /\.ts$/,
        use: {
          loader: 'ts-loader',
          options: {
            configFile: 'tsconfig.renderer.json',
          },
        },
        exclude: /node_modules/,
      },
      {
        test: /\.css$/,
        use: ['style-loader', 'css-loader'],
      },
      {
        test: /\.ttf$/,
        type: 'asset/resource',
      },
    ],
  },
  plugins: [
    new HtmlWebpackPlugin({
      template: './src/renderer/index.html',
    }),
    new MonacoWebpackPlugin({
      languages: ['cpp'],
      features: [
        'bracketMatching',
        'clipboard',
        'comment',
        'contextmenu',
        'cursorUndo',
        'find',
        'folding',
        'fontZoom',
        'gotoLine',
        'hover',
        'inlineCompletions',
        'indentation',
        'linesOperations',
        'linkedEditing',
        'multicursor',
        'parameterHints',
        'quickCommand',
        'rename',
        'smartSelect',
        'suggest',
        'wordHighlighter',
        'wordOperations',
      ],
    }),
  ],
  devtool: 'source-map',
};
